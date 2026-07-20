# Verification Reuse Shadow-Canary and Rollback Runbook

> Epic: **8z8q** — Verification dedup safety, telemetry, and cohort reporting proof
> Proposal: **czd9** — Wire the dark `verify_runs` fingerprint dedup
> Status: **Ready for Operations-owned rollout after the repository gates land**

---

## 1. Purpose and safety boundary

This repository-tracked runbook describes the safe, reversible rollout of final
verification reuse. It is a **code/docs artifact**, not a production credential
or an instruction to change feature behavior. Steps marked **Operations action**
require the appropriate production access and are not repository test acceptance
criteria.

The rollout order is deliberately:

1. writers first,
2. shadow observation while reuse is off,
3. one-project canary,
4. measured expansion, and
5. immediate rollback by disabling reuse.

The project-scoped control is the default-off setting:

```text
project.<project_id>.verify_run_reuse_enabled
```

The code reads this setting as `true` or `1`; an absent setting or any other
value is disabled. The corresponding source key construction is
`project.{project_id}.verify_run_reuse_enabled`.

**Important semantics:** disabling `verify_run_reuse_enabled` stops
consultation and reuse immediately for newly coordinated final verifications.
It does **not** stop eligible writers: canonical final verification is rebuilt
and eligible successful canonical passes continue to record `verify_runs`.
Merge-queue CI and project CI remain independent gates in every state; reuse
never replaces either gate.

---

## 2. Repository-checkable prerequisites (implementation gates)

Complete these gates before an Operations action. They are deterministic
repository evidence, not a claim about a live cohort:

- [ ] The writer path is landed and records only authoritative eligible final
  verification passes. It remains active when reuse is disabled.
- [ ] The default-off project setting and fail-closed consultation path are
  landed: a disabled, miss, stale, uncertain, or error result rebuilds the
  canonical final verification rather than reusing a result.
- [ ] The bounded telemetry/audit contract is landed. The lookup counter has
  exactly one `outcome` label and the writer counter has exactly one `outcome`
  label; correlation and diagnostic values are structured audit fields, not
  metric labels.
- [ ] The deterministic report contract and its committed artifacts are present:
  [`verify_dedup_report_contract.rs`](../../crates/djinn-db/tests/verify_dedup_report_contract.rs),
  [`metadata.json`](../../crates/djinn-db/tests/fixtures/verify_dedup_report_v1/metadata.json),
  [`events.json`](../../crates/djinn-db/tests/fixtures/verify_dedup_report_v1/events.json),
  and [`query.sql`](../../crates/djinn-db/tests/fixtures/verify_dedup_report_v1/query.sql).
- [ ] Run the exact deterministic repository command from the `server/`
  directory:

  ```bash
  cargo test -p djinn-db --test verify_dedup_report_contract -- --exact
  ```

  This is a fixture-only implementation gate. It proves the committed
  `verify_dedup_report_v1` arithmetic, exclusions, qualification, and
  fingerprint/C2 fail-closed contract; it does not run or qualify a production
  cohort.
- [ ] The deterministic safety matrices covering disabled mode, writer
  continuation, rollback behavior, C0/C1/C2 consistency, and independent CI
  gates are green in repository CI.

---

## 3. Telemetry and structured-audit inspection

### 3.1 Bounded counter outcomes

Inspect only these fixed metric series. There are no task, run, candidate,
command, fingerprint, version, or environment labels.

| Counter | Fixed `outcome` values | Meaning and action |
|---|---|---|
| `verify_cache_lookup_total` | `hit`, `miss`, `stale`, `error`, `disabled` | Every consultation emits one lookup result. `miss` and `stale` rebuild normally. `error` is actionable: inspect its structured audit event and rebuild/fallback remains expected. `disabled` confirms the default-off/rollback path. |
| `verify_run_record_total` | `stored`, `ineligible`, `error` | Every writer attempt emits one recording result. `stored` confirms an eligible canonical pass was persisted. `ineligible` is an expected non-recording reason to inspect in audit fields. `error` is actionable: inspect the structured audit event; it must not alter the canonical verification decision. |

Expected invariants during observation:

- Every consultation produces **exactly one** bounded lookup outcome.
- Every writer attempt produces **exactly one** bounded recording outcome.
- `Reused` is not a writer `stored` event; it must not inflate
  `verify_run_record_total`.
- Any lookup uncertainty or error is a rebuild signal, never permission to
  reuse. An unexpected `error` increase, an unexplained `stale` increase, or a
  missing expected bounded outcome is a stop-and-inspect signal.

### 3.2 Structured audit fields

Use the tracing/structured-log event associated with the outcome to drill down;
do not attempt to add any of the following as metric labels:

- `task_id`, `task_run_id`, and `verification_attempt_id`;
- candidate/persisted `verify_run_id`;
- ordered commands, covered checks, and required checks;
- `verification_input_fingerprint`, `manifest_version`, and
  `environment_identity_digest`;
- lookup/record `reason` and error `detail`.

For a hit, correlate the candidate/persisted run and input fingerprint with the
submission-time C2 evidence. For `error`, `stale`, or `ineligible`, preserve the
reason/detail, task/run identifiers, commands/checks, fingerprint, manifest
version, and environment identity digest in the incident record before
continuing the canonical rebuild path.

---

## 4. Ordered rollout checklist

### 4.1 Writer-first baseline

**Operations action:** Leave `project.<project_id>.verify_run_reuse_enabled`
absent or disabled for all projects. Deploy/observe writers first.

- [ ] Confirm `verify_cache_lookup_total{outcome="disabled"}` appears for
  configured final-verification coordination where consultation is reached.
- [ ] Confirm writers emit bounded `stored`, `ineligible`, or `error` outcomes
  as applicable, and inspect structured audit samples for the fields in §3.2.
- [ ] Confirm canonical final verification rebuilds while reuse is disabled.
- [ ] Confirm project CI and merge-queue CI continue as independent gates.

### 4.2 Shadow observation with reuse off

**Operations action:** Continue to observe writer-produced audit records and
report-shaped data while the project setting remains disabled. This is shadow
observation: it creates/inspects candidates without allowing consultation to
reuse them.

- [ ] Verify that disabled consultation does not block eligible writers.
- [ ] Inspect `stored` records and their structured C2/fingerprint evidence for
  completeness.
- [ ] Investigate `error`, unexpected `stale`, and `ineligible` signals using
  structured fields; do not infer root cause from the bounded counter alone.
- [ ] Do not enable a project until the repository gates in §2 and local
  operational readiness have been satisfied.

### 4.3 One-project canary

**Operations action:** Select one explicitly named project with a configured
final-verification plan, normal canonical verification traffic, available
metrics/log access, and an identified rollback owner. Record the project ID and
pre-enable timestamp.

- [ ] Set only `project.<project_id>.verify_run_reuse_enabled` to `true` (or
  `1`) for the selected project; leave every other project disabled.
- [ ] Verify the first consultations produce exactly one lookup outcome each.
  A `hit` is expected only with compatible current C0/C1/C2 evidence; `miss`,
  `stale`, and `error` must rebuild canonically.
- [ ] Inspect structured hit and fallback audit samples, including task/run and
  candidate IDs, commands/checks, fingerprint, manifest version, environment
  identity digest, and reason/detail where present.
- [ ] Confirm canonical writers still record eligible new canonical passes and
  project/merge-queue CI remains independent.
- [ ] Trigger §6 immediately for any safety uncertainty, unexpected audit
  mismatch, or operational error pattern.

### 4.4 Expansion

**Operations action:** Expand one project at a time only after the canary has
stable bounded telemetry and auditable evidence. Keep the project-scoped flag;
do not turn this into a global default-on rollout.

- [ ] Record each project ID, enable timestamp, owner, and rollback owner.
- [ ] Re-run the §3 inspection after every expansion step.
- [ ] Keep the ability to disable each project independently.
- [ ] Pause expansion and roll back affected projects for uncertainty/error
  signals; an increase in `error`, unexplained `stale`, or any audit/C2
  inconsistency is sufficient to stop.

---

## 5. Deterministic report contract and live ownership

The exact committed report-query version is **`verify_dedup_report_v1`**:

```text
server/crates/djinn-db/tests/fixtures/verify_dedup_report_v1/
├── metadata.json
├── events.json
└── query.sql
```

`query.sql` is the controlled SQL query artifact and `metadata.json` pins the
query version, non-overlapping `pre` and `post` half-open windows, declared
infrastructure-wide outage intervals, a strict ratio limit below `1.5`, and
minimums of 50 completed task runs plus 30 distinct fingerprints **per cohort**.
The query publishes the query version, canonical-build-execution numerator,
distinct-fingerprint denominator, ratio, cohort counts, and exclusions for
`project_ci`, `merge_queue_ci`, `cancelled_before_first_command`,
`infrastructure_wide_outage`, and `missing_fingerprint`.

### Operations-owned live target — not repository acceptance

**Operations action:** For rollout decisions, run the deployed equivalent of
committed `verify_dedup_report_v1/query.sql` against a live, non-overlapping
seven-day observation design and retain the query version and evidence. The
live target is a ratio strictly below `1.5` after the declared exclusions, with
the configured cohort minima (at least 50 completed task runs and 30 distinct
fingerprints in each qualifying cohort). Operations owns cohort selection,
production data access, live results, and the decision to expand or stop.

Repository CI does **not** accept or reject the rollout based on an operator
action, a live seven-day ratio, or a live cohort result. It instead gates the
deterministic fixture/query version and safety matrices: exact numerator and
denominator arithmetic, exclusions, minimum qualification logic, rejection at
exactly `1.5`, acceptance below it, and failure of an audited reuse whose stored
fingerprint differs from submission-time C2.

---

## 6. Immediate rollback

### 6.1 Disable reuse (the immediate safety action)

**Operations action:** Set the affected project's
`project.<project_id>.verify_run_reuse_enabled` value to a disabled value (or
remove the setting). This is the immediate rollback action.

After disabling:

- consultation/reuse stops for new final-verification coordination;
- canonical final verification rebuilds rather than reusing a candidate;
- eligible writers continue to record authoritative canonical passes;
- merge-queue CI and project CI remain independent gates; and
- existing stored verification rows may remain for audit/history but are no
  longer consulted while the flag is disabled.

- [ ] Disable the affected project flag.
- [ ] Verify `verify_cache_lookup_total{outcome="disabled"}` and structured
  audit samples for newly coordinated work.
- [ ] Verify canonical rebuild and eligible writer continuation.
- [ ] Verify project CI and merge-queue CI remain independent.
- [ ] Preserve metric and structured-audit evidence for the incident and keep
  the project disabled until the cause is resolved.

### 6.2 Disabling reuse is not removing a configured plan

Do **not** remove or alter the project's configured final-verification plan to
perform a reuse rollback. Disabling `verify_run_reuse_enabled` only prevents
consultation/reuse and preserves the canonical final-verification workflow and
writers. Removing the configured plan is a separate configuration change that
can suppress final-verification work entirely; it is not an equivalent rollback
and must not be used as the immediate safety response.

---

## 7. References

- `server/crates/djinn-slot/src/final_verification.rs` — project gate,
  fail-closed consultation, canonical fallback, and writer path
- `server/crates/djinn-slot/src/final_verification/telemetry_contract_tests.rs`
  — bounded counter and structured-audit contract
- `server/crates/djinn-telemetry/src/lib.rs` — fixed lookup/record outcomes
- `server/crates/djinn-db/tests/verify_dedup_report_contract.rs` — deterministic
  report model and safety assertions
- `server/crates/djinn-db/tests/fixtures/verify_dedup_report_v1/` — committed
  `verify_dedup_report_v1` query, metadata, and fixtures
- Memory: `design/8z8q-roadmap`
