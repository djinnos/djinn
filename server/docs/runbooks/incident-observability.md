# Incident observability operator runbook

This runbook operates the opt-in chart monitoring stack and node-local durable
pod-log collector. Repository checks render deterministic Helm/schema/docs
inputs only; they do **not** provision a disk, create credentials, activate an
external webhook provider, or prove a real page. Those are operator gates.

## Prerequisites and production gates

Before enabling `monitoring.enabled` or `logCollector.enabled`:

1. Prepare a dedicated node-local retained-log directory at the configured
   `logCollector.storePath` (default `/var/lib/djinn-observability`). It must
   already exist on every selected node, have capacity for the agreed retention
   policy, and be owned/writable by UID/GID `10002`. The chart deliberately uses
   `hostPath.type: Directory`, so it will not create or repair this disk.
2. Create the **existing** Kubernetes Secret named by
   `monitoring.alertmanager.webhookSecret`. Its configured key contains the full
   HTTPS receiver URL. Helm references it; it does not create the Secret or the
   external provider integration.
3. Schedule the collector only on nodes whose disk preparation and ownership
   have been checked, and confirm its source mount (`/var/log/pods`) and store
   mount are distinct. Vector reads the source; only the rotator writes `/store`.
4. Activate the receiver/provider using the provider's change control. A
   controlled real-page canary below is an operator-owned production gate, not
   repository proof.

Repository verification is `bash deploy/helm/djinn/tests/incident-observability-contract.sh`.
It validates schema/render invariants and composes the existing log collector
contract; it does not contact a cluster or provider.

## Safe retained-log retrieval

Run `scripts/djinn-observability-logs` on the node holding the retained store,
preferably through approved node access:

```sh
scripts/djinn-observability-logs --namespace <namespace> --pod-uid <uuid> \
  --container <container>
```

Use `--pod-uid`, not the pod-name compatibility fallback, to avoid ambiguity.
The helper reads compressed completed segments (`.jsonl.gz`) with `gzip -cd` and
reads an active segment (`.jsonl.active`) as a size-bounded snapshot. Active
output can exclude bytes appended after the snapshot; rerun it rather than
copying, truncating, or modifying the live file. A corrupt gzip segment is a
storage incident: preserve its path and error, do not overwrite it. Treat the
store as evidence and never run cleanup while collecting incident data.

## Triage alerts and store pressure

### `DjinnDispatchWithoutCompletion`

This means at least the configured number of task-run dispatches occurred in
15 minutes without a durable worker completion for five minutes. Check server
and worker restarts, pending/failed Jobs, task-run lifecycle logs, and the
retained logs for affected pod UIDs. Preserve dispatch/completion counter values
and a time window before retrying work; do not infer completion from a pod exit.

### `DjinnServerMemoryPressure`

Server RSS exceeded the configured fraction of `djinn_server_memory_limit_bytes`
for ten minutes. Capture RSS, configured limit, restart/OOM events, and active
workload. Reduce load or roll back the triggering workload/configuration through
normal change control; do not silently raise the threshold as remediation.

### `DjinnServerMetricsMissing`

The `djinn-server` scrape has been unavailable for five minutes. First check the
server pod, Service endpoints, `/metrics`, DNS/network policy, and Prometheus
configuration. This alert inhibits memory/store/rotator symptom alerts for the
same namespace; record the impairment before acting on their absence.

### `DjinnLogStoreUnavailable` and store-pressure states

This fires when the rotator reports an unwritable store or evictions. Check the
dedicated disk's free space, inode use, mount/read-only errors, UID/GID `10002`
ownership, rotator health, and `djinn_log_store_writable` plus eviction counter.
Preserve impacted `.jsonl.active`/`.jsonl.gz` paths, then stop or roll back
collection if it risks further loss. Disk expansion, remounting, and ownership
repair are operator actions, not chart behavior.

### `DjinnLogRotatorMissing`

The 30-second rotator scrape target has been absent for two minutes. Check the
collector DaemonSet scheduling, selected node, rotator container, localhost
`8687` ingest, and rotator metrics Service on `9091`. Confirm Vector has not
been granted the store mount; repair or roll back only the collector after
recording the gap.

### `Watchdog`

Watchdog should fire continuously after one minute and be delivered every minute
by its dedicated route. Check Prometheus rule evaluation, Alertmanager route,
Secret-mounted URL file, receiver/provider status, and the receiver's dead-man
history. Its loss is a paging impairment even when no application alert is
present; open/append an impairment record immediately.

## Muted-to-live canary

1. Enable the chart with the receiver/provider muted or directed to a controlled
   test receiver. Verify both 30-second scrape jobs, rule loading, Secret mount,
   retained store health, and Watchdog payload/cadence without notifying people.
2. Exercise a controlled alert path only in the approved canary scope and retain
   timestamps, payload, Alertmanager status, and receiver evidence. Do not use a
   customer-impacting failure as a test.
3. Move the provider from muted to a limited live canary route. Confirm one
   controlled page and acknowledgement according to the provider's procedure.
4. Expand routing only after the live canary and Watchdog continuity are recorded.
   Re-mute or roll back on unexpected destination, duplicate page, missing page,
   or receiver error.

## Component rollback and impairment record

Use the narrowest rollback; preserve evidence and record the UTC start/end,
affected namespace/nodes, alert names, missing telemetry/pages, operator,
configuration/chart revision, receiver/provider status, and follow-up owner.

| Component | Rollback / containment | Record as impairment |
| --- | --- | --- |
| Logs / collector | Set `logCollector.enabled=false` or roll back only the DaemonSet after preserving store evidence. | Missing/evicted segments, unwritable store, collector coverage gap. |
| Panic / capture | Disable or roll back the capture change through its owning server release; retain existing panic evidence. | Lost/partial capture, hook failure, affected task-run IDs. |
| Retention | Return to the prior retention policy or suspend the retention change; never delete evidence to recover capacity. | Early deletion, quota/age transition, affected paths. |
| Monitoring | Set `monitoring.enabled=false` or roll back Prometheus/Alertmanager independently. | Missing scrape/rule/evaluation interval and monitoring blind window. |
| Routing | Re-mute the provider or restore the prior Alertmanager Secret/route revision. | Wrong recipient, receiver failure, duplicate/missing notification. |
| Watchdog | Restore the prior rule/route after checking receiver dead-man handling. | Every Watchdog absence, cadence break, and synthetic/real-page result. |

A rollback does not erase the paging impairment: attach the record to the
incident/change ticket and explicitly state whether a real external provider
page was performed. Repository tests alone never establish that fact.
