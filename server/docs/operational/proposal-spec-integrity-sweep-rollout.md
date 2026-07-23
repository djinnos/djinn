# Proposal spec integrity sweep rollout and rollback checklist

This is the deployment contract for the retroactive proposal integrity doctor
sweep. It is an operational checklist, not evidence that a production canary
has run. The sweep never edits proposal bodies.

## Preconditions and exact rollout order

Complete the following stages in this exact order. Do not enable a later stage
until the prior stage is deployed and its repository-level checks are green.

1. **Shared linter and fixtures.** Deploy the versioned shared proposal spec
   linter and its deterministic fixture corpus.
2. **Additive migrations.** Apply
   `server/crates/djinn-db/migrations_postgres/137_proposal_revision_lint_results.sql`.
   It additively creates `proposal_revision_lint_results` and the immutable
   `doctor_findings.deduplication_key` uniqueness surface. Apply migration 145
   as its existing additive compatibility companion, but do not use its
   `active_key`/status reconciliation for integrity history.
3. **Repository invariant.** Deploy the proposal-revision repository boundary
   that writes the immutable lint result for each accepted revision.
4. **API/DoR/tribunal consumers.** Deploy API/schema publication and the
   Definition of Ready and tribunal consumers of the lint result.
5. **UI.** Deploy the current-head and immutable historical lint-result UI.
6. **Disabled sweep deployment.** Deploy the coordinator sweep with
   `proposal_spec_integrity_sweep_v1` disabled. Its process configuration is
   `DJINN_PROPOSAL_SPEC_INTEGRITY_SWEEP_V1=false`; this must remain the default
   on every non-canary coordinator.
7. **Canary observation.** Use the bounded observation procedure below while
   the flag remains the only mechanism that starts retroactive load.
8. **Sweep enablement.** Only after the observation gate is satisfied, set
   `DJINN_PROPOSAL_SPEC_INTEGRITY_SWEEP_V1=true` for the elected canary
   coordinator scope. This enables `proposal_spec_integrity_sweep_v1`.

## Bounded canary observation

This procedure deliberately makes no production-canary success claim. Schedule
a 30-minute observation window for one elected coordinator canary scope. Keep
the production bounds unchanged: a leader tick reads ascending proposal-head
pages and consumes at most 10 pages per tick. Do not widen the page limit,
parallelize the canary, or enable the flag on another coordinator during this
window.

During the window, observe and record:

- the flag/configuration value and elected-leader identity;
- scan/page rate and bounded tick duration;
- per-proposal lint or persistence warnings (they retry on a later tick) and
  whether unrelated coordinator work continues;
- conflict-safe materialization counts and unique
  `proposal_spec_integrity_v1:<proposal_id>:<revision_seq>:<linter_version>`
  findings, including no duplicate key growth on a rerun; and
- stale-head discards and the absence of proposal-body writes.

The observation gate is an operator decision based on those records. This
checked-in artifact and its source-contract test require neither Kubernetes,
production credentials, a live canary, nor post-land CI observation.

## Immediate rollback and retained history

`proposal_spec_integrity_sweep_v1` is the immediate retroactive-load kill
switch. To stop new retroactive scans and retained-body loads, set
`DJINN_PROPOSAL_SPEC_INTEGRITY_SWEEP_V1=false` in the elected coordinator scope
and roll/reload that scope according to normal configuration delivery. The
disabled gate occurs before sweep-source construction; it does not scan or load
proposal bodies.

Rollback is configuration/deployment rollback only. It must retain all of the
following durable history:

- every `proposal_revision_lint_results` row;
- every immutable `doctor_findings.deduplication_key` value;
- all immutable lint rows; and
- all historical `proposal_spec_integrity_v1` doctor findings.

Do not delete proposal lint rows, doctor findings, proposal revisions, or
proposal bodies as part of rollback. Do not add a destructive down migration or
cleanup job. Migration 145's `doctor_findings.active_key` reconciliation is for
unrelated active-finding state; it must not reconcile, resolve away, or delete
the immutable integrity findings keyed by `deduplication_key`.

After disabling, observe that no new sweep scan/load activity starts, while
existing lint rows and historical integrity findings remain queryable through
the repository, API, and UI. Re-enabling later resumes the additive,
conflict-safe materialization behavior; it does not erase prior evidence.
