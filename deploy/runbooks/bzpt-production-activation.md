# bzpt production activation gate

This runbook is the repository-owned operational boundary for a **future** bzpt
production activation. It is intentionally a procedure and record template: it
does not require a live cluster, credentials, an actual cutover, or production
evidence while this repository change is reviewed.

## Scope and independent delivery

This gate serializes **only** bzpt production activation behind d308's closed
activation-or-rollback observation window. It does not create a coordinator or
epic dependency between d308 and bzpt.

**Implementation, review, merge, and non-production validation remain independent of this production-activation gate.**

## Durable operation record

Complete these fields during the future operation; do not substitute fixture or
repository-delivery values for operator evidence.

| Field | Operator record |
| --- | --- |
| d308 activation-or-rollback observation-window close timestamp | `D308_OBSERVATION_WINDOW_CLOSE_TIMESTAMP: <UTC ISO-8601 timestamp>` |
| Selected Helm release revision | `SELECTED_HELM_RELEASE_REVISION: <revision number>` |
| Deployment outcome | `DEPLOYMENT_OUTCOME: activation` or `DEPLOYMENT_OUTCOME: rollback` |
| Retained upgrade/rollback transcript link | `RETAINED_UPGRADE_OR_ROLLBACK_TRANSCRIPT_LINK: <durable URL>` |
| Retained Helm-history transcript link | `RETAINED_HELM_HISTORY_TRANSCRIPT_LINK: <durable URL>` |

## Closed d308 observation-window prerequisite

Before **any** bzpt production-activation step, cite the retained d308 record
and confirm that its activation-or-rollback observation window is closed:

`D308_OBSERVATION_WINDOW_STATUS: closed`

Record its close timestamp in the durable operation record above. Do not start,
retry, or advance a bzpt production activation while this prerequisite is
unmet.

## bzpt production activation procedure

### BZPT_PRODUCTION_ACTIVATION_STEP: select and execute the outcome

After the closed-window prerequisite is recorded, set the release context:

```bash
RELEASE=<production-release>
NAMESPACE=<production-namespace>
CHART=<approved-chart-reference>
PREPARATION_REVISION=<previous-complete-preparation-revision>
```

For an activation, retain the complete command transcript showing success:

```bash
helm upgrade --atomic --wait "$RELEASE" "$CHART" --namespace "$NAMESPACE"
```

If the operation instead ends in rollback, retain the complete command
transcript showing success:

```bash
helm rollback --wait "$RELEASE" "$PREPARATION_REVISION" --namespace "$NAMESPACE"
```

Set `DEPLOYMENT_OUTCOME` to exactly `activation` or `rollback` and attach the
corresponding durable URL as
`RETAINED_UPGRADE_OR_ROLLBACK_TRANSCRIPT_LINK`.

### BZPT_PRODUCTION_ACTIVATION_STEP: verify and retain Helm history

After the successful activation or rollback command, run and retain this
transcript:

```bash
helm history "$RELEASE" --namespace "$NAMESPACE"
```

Set `SELECTED_HELM_RELEASE_REVISION` to the selected revision. The retained
Helm-history transcript must show that selected revision with status `deployed`;
attach its durable URL as
`RETAINED_HELM_HISTORY_TRANSCRIPT_LINK`. A revision that is not shown as
`deployed` is not activation evidence.

## Completion record

The bzpt release owner records the closed d308 observation-window citation,
close timestamp, selected deployed revision, outcome, and both transcript links
in the production change record. This evidence is produced at the future
cutover; it is not a repository-delivery acceptance artifact.
