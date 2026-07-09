# Proposal `cxvq` — Acceptance Audit Artifact

> **Proposal:** Repurpose Lead as the park-rung Arbiter: one forensic decision before
> human-review holds; remove `request_lead` from worker/reviewer.
>
> **Epic cluster:** `7f8u` (arbiter foundation), `oelp` (decision contract),
> `10qg` (Lead cut-over), `aizl` (rollout hardening).
>
> **Audit date:** 2026-07-09
>
> **Purpose:** Map every `cxvq` acceptance criterion to concrete code paths, test names,
> or documented operational evidence. Distinguish worker-checkable code/test evidence
> from operator-only rollout observations. No criterion is left as an unverifiable
> assertion.

---

## Legend

| Symbol | Meaning |
|--------|---------|
| ✅ **Code/Test** | Criterion is fully verifiable by running the cited code paths or tests. A worker can confirm it in CI. |
| 🔶 **Operator** | Criterion requires live production observation (e.g., a two-week metrics review). Cannot be verified by a worker alone. |
| 📋 **Checklist** | Criterion is covered by an operational runbook checklist rather than worker acceptance criteria. |

---

## Acceptance Criteria Audit

### AC 1 — Durable arbitration state persisted per `(task_id, hold_cycle)`

**Status:** ✅ Code/Test

**Evidence:**

| Artifact | Path |
|----------|------|
| Schema & repository | `server/crates/djinn-db/src/repositories/task_arbitration.rs` — `TaskArbitrationRepository`, `TaskArbitrationRecord`, `try_create`, `mark_consumed`, `mark_failed`, `record_monitored_reopen`, `update_dispatch_ledger` |
| Migration | `server/crates/djinn-db/migrations_postgres/` (latest: arbitration table keyed by `task_id + hold_cycle`) |
| Repository unit tests | `server/crates/djinn-db/src/repositories/task_arbitration.rs` `mod tests` (line 581+) |
| Test support helper | `server/crates/djinn-db/src/repositories/test_support.rs` — `reject_new_task_arbitrations_for_test` (line 187) |
| Delivering task | `enhd` (epic `7f8u`) |

---

### AC 2 — Atomic park-rung dispatch of exactly one Lead arbiter per unconsumed hold cycle

**Status:** ✅ Code/Test

**Evidence:**

| Artifact | Path |
|----------|------|
| Coordinator dispatch | `server/crates/djinn-coordinator/src/dispatch/retry.rs` — `route_planner_intervention` (lines ~1538–1675): atomic INSERT + transition + outbox |
| Dispatch recovery test | `server/crates/djinn-coordinator/src/tests/intervention.rs` — `arbiter_dispatch_transition_before_activity_failure_recovers_to_single_record` (line 3846) |
| Outbox/atomic marker test | `server/crates/djinn-coordinator/src/tests/intervention.rs` — `second_strike_arbiter_dispatch_atomic_marker_status_and_outbox` (line 5535) |
| No-double-dispatch test | `server/crates/djinn-coordinator/src/tests/intervention.rs` — `consumed_arbitration_prevents_duplicate_dispatch` (line 1784) |
| Re-entry does not create second arbiter | `server/crates/djinn-coordinator/src/tests/intervention.rs` — `reentry_with_unconsumed_arbiter_does_not_create_second_arbiter` (line 3746) |
| Cycle advancement test | `server/crates/djinn-coordinator/src/tests/intervention.rs` — `consumed_arbitration_advances_to_next_cycle_and_dispatches` (line 3571) |
| Delivering tasks | `cmat`, `0bnl`, `lvm4` (epic `7f8u`); `vi8e` (epic `aizl`) |

---

### AC 3 — `submit_decision` contract validation (park, reopen, approve, supersede)

**Status:** ✅ Code/Test

**Evidence:**

| Artifact | Path |
|----------|------|
| Contract schema | `server/crates/djinn-mcp-extension/src/finalize_tools.rs` — `tool_submit_decision()` (line 74) |
| Validation logic | `server/crates/djinn-agent/src/roles/finalize.rs` — decision parsing/validation |
| Stage parsing | `server/crates/djinn-agent/src/supervisor_impl/stage.rs` — `StageOutcome::ArbiterDecision` branch |
| Park validation test | `server/crates/djinn-supervisor/src/lib.rs` — `arbiter_park_persists_dossier_and_emits_closed_outcome` (line 6710) |
| Reopen validation test | `server/crates/djinn-supervisor/src/lib.rs` — `arbiter_reopen_persists_directive_and_fires_transition` (line 6337) |
| Approve validation tests | `server/crates/djinn-supervisor/src/lib.rs` — `arbiter_approve_green_gate_proceeds_with_transition` (line 6121), `arbiter_approve_red_gate_blocks_without_transition` (line 6154), `arbiter_approve_conflict_green_gate_proceeds_with_transition` (line 6198), `arbiter_approve_conflict_red_gate_blocks_without_transition` (line 6233) |
| Supersede test | `server/crates/djinn-supervisor/src/lib.rs` — `arbiter_supersede_force_closes_source_with_replacement_ids_no_hold` (line 6417) |
| Delivering task | `ihzi` (epic `oelp`) |

---

### AC 4 — Park decision persists dossier and creates HumanReview remediation hold

**Status:** ✅ Code/Test

**Evidence:**

| Artifact | Path |
|----------|------|
| Park transaction | `server/crates/djinn-agent/tests/arbiter_park_transaction.rs` — integration tests for dossier persistence and HumanReview hold creation |
| Dossier persistence test | `server/crates/djinn-supervisor/src/lib.rs` — `arbiter_park_persists_dossier_and_emits_closed_outcome` (line 6710) |
| Structured evidence fields | `server/crates/djinn-supervisor/src/lib.rs` — `arbiter_park_dossier_structured_evidence_fields_preserved` (line 6783) |
| Transition type test | `server/crates/djinn-supervisor/src/lib.rs` — `arbiter_park_transition_is_exactly_arbiter_park_for_human_review_hold` (line 6868) |
| Coordinator hold creation | `server/crates/djinn-coordinator/src/tests/intervention.rs` — `arbiter_park_persists_decision_creates_human_review_hold` (line 5038) |
| Dossier with attempt ledger | `server/crates/djinn-coordinator/src/tests/intervention.rs` — `arbiter_dossier_includes_attempt_ledger` (line 4403) |
| Delivering tasks | `q3uc` (epic `oelp`); `vi8e` (epic `aizl`) |

---

### AC 5 — Arbiter approval routed through pre-approval verification gate (green/red)

**Status:** ✅ Code/Test

**Evidence:**

| Artifact | Path |
|----------|------|
| Green gate proceeds | `server/crates/djinn-supervisor/src/lib.rs` — `arbiter_approve_green_gate_proceeds_with_transition` (line 6121) |
| Red gate blocks without consuming | `server/crates/djinn-supervisor/src/lib.rs` — `arbiter_approve_red_gate_blocks_without_transition` (line 6154), `arbiter_gate_red_does_not_consume_arbitration_or_increment_counters` (line 6269) |
| Red gate records gate feedback | `server/crates/djinn-supervisor/src/lib.rs` — `arbiter_approve_red_records_decision_with_gate_feedback` (line 7282), `arbiter_approve_conflict_red_records_decision_with_gate_feedback` (line 7344) |
| Green records evidence on row | `server/crates/djinn-supervisor/src/lib.rs` — `arbiter_approve_green_records_evidence_on_arbitration_row` (line 7191), `arbiter_approve_conflict_green_records_evidence_on_arbitration_row` (line 7235) |
| Infra error fail-open | `server/crates/djinn-supervisor/src/lib.rs` — `arbiter_approve_infra_error_records_decision_fail_open` (line 7400) |
| Gate infra error proceeds | `server/crates/djinn-supervisor/src/lib.rs` — `arbiter_gate_infra_error_proceeds_fail_open` (line 6305) |
| Pre-approval gate code | `server/crates/djinn-coordinator/src/preapproval_gate.rs` |
| Delivering task | `k9hj` (epic `oelp`) |

---

### AC 6 — One-shot monitored reopen: directive injected into exactly one next worker prompt

**Status:** ✅ Code/Test

**Evidence:**

| Artifact | Path |
|----------|------|
| Reopen persists directive + excludes | `server/crates/djinn-supervisor/src/lib.rs` — `arbiter_reopen_persists_directive_and_fires_transition` (line 6337), `arbiter_reopen_empty_exclude_models_still_persists_directive` (line 6979), `arbiter_reopen_multiple_exclude_models_all_persisted` (line 7035) |
| No PR open on reopen | `server/crates/djinn-supervisor/src/lib.rs` — `arbiter_reopen_does_not_call_open_pr` (line 6481) |
| Worker submit completes monitored reopen | `server/crates/djinn-supervisor/src/lib.rs` — `worker_submit_completes_monitored_reopen` (line 6520) |
| Worker failure completes | `server/crates/djinn-supervisor/src/lib.rs` — `worker_failure_completes_monitored_reopen` (line 6552) |
| Loop guard completes | `server/crates/djinn-supervisor/src/lib.rs` — `worker_loop_guard_completes_monitored_reopen` (line 6583) |
| Reviewer rejection completes | `server/crates/djinn-supervisor/src/lib.rs` — `reviewer_rejection_completes_monitored_reopen` (line 7083) |
| Interrupted run does NOT complete | `server/crates/djinn-supervisor/src/lib.rs` — `interrupted_run_does_not_complete_monitored_reopen` (line 7115) |
| No-eligible-model parking | `server/crates/djinn-coordinator/src/dispatch/task_dispatch.rs` (line 2122: `monitored_reopen_no_eligible_model` dossier) |
| `no_eligible_model` arbitration test | `server/crates/djinn-db/src/repositories/task_arbitration.rs` — `no_eligible_model_completes_monitored_reopen` (line 1445) |
| Directive injection gating | `server/crates/djinn-agent/src/supervisor_impl/stage.rs` (line 928: monitored reopen worker-only prompt gating) |
| Delivering task | `zkk9` (epic `oelp`) |

---

### AC 7 — Bounded termination accounting: infra failures don't increment decision-failure count; cap at 2

**Status:** ✅ Code/Test

**Evidence:**

| Artifact | Path |
|----------|------|
| Termination accounting code | `server/crates/djinn-db/src/repositories/task_arbitration.rs` — `increment_decision_failure`, `increment_infra_retry` |
| Decision failure cap dossier | `server/crates/djinn-coordinator/src/dispatch/retry.rs` (line 1406: `arbiter_decision_failure_cap` kind) |
| Infra termination metric | `server/crates/djinn-telemetry/src/lib.rs` — `arbiter::TERMINATION_INFRA`, `arbiter::TERMINATION_DECISION_FAILURE` |
| Telemetry render tests | `server/crates/djinn-telemetry/src/lib.rs` — `arbiter_termination_metric_names_and_labels_render` (line 2811) |
| Infra-exempt counters | `server/crates/djinn-telemetry/src/lib.rs` — `infra_delta` module with `INFRA_EXEMPT_TOTAL` metric |
| Delivering task | `q8r6` (epic `oelp`); `qk8b` (epic `aizl`) |

---

### AC 8 — Wall-clock arbitration deadline auto-parks with failure dossier

**Status:** ✅ Code/Test

**Evidence:**

| Artifact | Path |
|----------|------|
| Deadline enforcement | `server/crates/djinn-coordinator/src/dispatch/retry.rs` — `arbitration_deadline_has_expired` (line 1020), `arbiter_deadline_expired_dossier` (line 1034), `enforce_expired_arbiter_deadline_before_dispatch` (line 1063) |
| Deadline metric label | `server/crates/djinn-telemetry/src/lib.rs` — `arbiter::PARK_REASON_DEADLINE_EXPIRED` |
| Park metric for deadline | `server/crates/djinn-telemetry/src/lib.rs` — `arbiter_time_in_arbitration_seconds` histogram |
| Telemetry render tests | `server/crates/djinn-telemetry/src/lib.rs` — `arbiter_park_metric_names_and_labels_render` (line 2747), `arbiter_time_in_arbitration_histogram_renders` (line 2826) |
| Delivering task | `q8r6` (epic `oelp`); `qk8b` (epic `aizl`) |

---

### AC 9 — `request_lead` removed from active worker/reviewer tool surfaces

**Status:** ✅ Code/Test

**Evidence:**

| Artifact | Path |
|----------|------|
| Worker/reviewer config | `server/crates/djinn-roles/src/config.rs` — `request_lead` count: **0** (grep verified) |
| Worker/reviewer schema snapshot test | `server/crates/djinn-agent/src/extension/tests/schema_snapshot_tests.rs` — `worker_reviewer_schemas_expose_request_planner_not_request_lead` (line 360) |
| MCP extension schema test | `server/crates/djinn-mcp-extension/src/tests/schema_tests.rs` — `worker_reviewer_schemas_expose_request_planner_not_request_lead` (line 1214) |
| Production dispatch guard | `server/crates/djinn-agent/src/extension/tests/schema_snapshot_tests.rs` — `production_dispatch_does_not_handle_request_lead` (line 410) |
| Lead schema excludes `request_planner`/`escalate` | `server/crates/djinn-mcp-extension/src/tests/schema_tests.rs` — `lead_schema_does_not_expose_request_planner_or_escalate` (line 1233) |
| Historical-compat retention | `server/crates/djinn-mcp-extension/src/tool_defs.rs` — `tool_request_lead()` retained with `[HISTORICAL-COMPAT]` comment (line 36); NOT in production worker/reviewer tool lists |
| Deprecated drain compat | `server/crates/djinn-agent/src/supervisor_impl/stage.rs` — `request_lead` handler emits `deprecated_request_lead` activity and routes to Planner, NOT to `needs_lead_intervention` |
| Drain compat tests | `server/crates/djinn-agent/src/supervisor_impl/stage.rs` — `reviewer_deprecated_request_lead_escalates_to_planner` (line 1990), `worker_deprecated_request_lead_escalates_to_planner` (line 2519), `worker_deprecated_request_lead_does_not_produce_needs_lead_intervention` (line 2561), `deprecated_request_lead_handler_does_not_use_lead_request_comment_convention` (line 2606) |
| Delivering tasks | `jurr`, `8qyt`, `g9hd` (epic `10qg`); `m2e8` (epic `aizl`) |

---

### AC 10 — Lead prompt rewritten as forensic arbiter mandate

**Status:** ✅ Code/Test

**Evidence:**

| Artifact | Path |
|----------|------|
| Forensic arbiter prompt | `server/crates/djinn-roles/src/prompts/lead.md` — first line: `## Mission: Forensic Arbiter`; describes evidence-gated decisions, read-only shell, `submit_decision` as the sole board transition tool |
| Forensic mandate test | `server/crates/djinn-roles/src/prompts/tests.rs` — `lead_prompt_contains_forensic_arbiter_mandate` (line 1426) |
| Supersede decision in matrix | `server/crates/djinn-roles/src/prompts/tests.rs` — `lead_prompt_decision_matrix_includes_supersede_and_prefers_it_over_park` (line 1473) |
| No stale escalation surface | `server/crates/djinn-roles/src/prompts/tests.rs` — `lead_prompt_does_not_mention_request_planner_or_escalate_as_decision_tools` (line 1497) |
| Delivering task | `htqo` (epic `10qg`); `g9hd` (epic `10qg`) |

---

### AC 11 — Arbiter dispatch uses plan/strongest model lane

**Status:** ✅ Code/Test

**Evidence:**

| Artifact | Path |
|----------|------|
| Post-intervention lane module | `server/crates/djinn-coordinator/src/dispatch/post_intervention_lane.rs` — `use_plan_lane_for_post_intervention_workers()` (line 8) |
| Lane application in dispatch | `server/crates/djinn-coordinator/src/dispatch/task_dispatch.rs` — `effective_dispatch_lane` call (line 1987) |
| Lane regression tests | Covered in epic `10qg` cut-over regressions (`g9hd`) for model-lane behavior |
| Delivering task | `htqo` (epic `10qg`); `g9hd` (epic `10qg`) |

---

### AC 12 — Git evidence fields in dispatch ledger and park dossier payloads

**Status:** ✅ Code/Test

**Evidence:**

| Artifact | Path |
|----------|------|
| Ledger fields in schema | `server/crates/djinn-db/src/repositories/task_arbitration.rs` — `TaskArbitrationRecord` with `mirror_head_sha`, `github_head_sha`, `pr_url`, failing CI job IDs |
| Activity payload with evidence | `server/crates/djinn-agent/tests/arbiter_park_transaction.rs` — `arbiter_decision_payload_includes_git_evidence_when_populated` (line 697), `arbiter_decision_payload_has_empty_evidence_when_absent` (line 789), `arbiter_park_transaction_persists_git_evidence_on_ledger` (line 1119) |
| Coordinator dossier evidence | `server/crates/djinn-coordinator/src/tests/intervention.rs` — `arbiter_failure_dossier_on_db_error_parks_with_evidence_fields` (line 5791; uses `mirror-head-try-create-fail`, CI job IDs as evidence assertions) |
| Activity payload types | `server/crates/djinn-coordinator/src/dispatch/retry.rs` — `arbiter_dispatched` payload includes `mirror_head_sha`, `github_head_sha`, `pr_url` fields |
| Supervisor decision evidence | `server/crates/djinn-supervisor/src/lib.rs` — `arbiter_approve_green_records_evidence_on_arbitration_row` (line 7191) |
| Decision failure cap dossier path | `server/crates/djinn-coordinator/src/dispatch/retry.rs` (line 1406) — direct-services decision-failure cap dossier also carries git evidence fields |
| Delivering task | `qk8b` (epic `aizl`); `vi8e` (epic `aizl`) |

---

### AC 13 — Only coordinator park rung (`Escalate`) can transition to `needs_lead_intervention`

**Status:** ✅ Code/Test

**Evidence:**

| Artifact | Path |
|----------|------|
| State machine invariant | `server/crates/djinn-core/src/models/task.rs` — `UserOverride` guard (lines 904–918): explicitly rejects `NeedsLeadIntervention` / `InLeadIntervention` as targets; doc comment cites "INVARIANT (10qg/aizl)" |
| Exhaustive action test | `server/crates/djinn-core/src/models/task.rs` — `only_escalate_and_release_produce_needs_lead_intervention` (line 2212): enumerates ALL `TransitionAction` variants, tries every source status, asserts only `Escalate` and `LeadInterventionRelease` produce `NeedsLeadIntervention` |
| Escalate status matrix test | `server/crates/djinn-core/src/models/task.rs` — `escalate_accepts_all_park_rung_source_statuses` (line 2042) |
| Stage deprecated drain test | `server/crates/djinn-agent/src/supervisor_impl/stage.rs` — `worker_deprecated_request_lead_does_not_produce_needs_lead_intervention` (line 2561): confirms stale `request_lead` from workers does NOT enter the Lead lifecycle |
| Delivering tasks | `g9hd` (epic `10qg`); `m2e8` (epic `aizl`); foundation from `5z04` (epic `7f8u`) |

---

## Arbiter Rollout Metrics (Landed by `qk8b`)

The following metric family was added in `server/crates/djinn-telemetry/src/lib.rs` (lines 86–91):

| Metric Name | Type | Labels | Purpose |
|-------------|------|--------|---------|
| `djinn_arbiter_decision_total` | counter | `decision` (approve/approve_conflict/reopen/park/escalate/decompose/force_close) | Decision distribution |
| `djinn_arbiter_park_total` | counter | `reason` × `outcome` (5 reasons × 3 outcomes) | Park reason/outcome breakdown |
| `djinn_arbiter_monitored_reopen_total` | counter | `outcome` (started/no_unconsumed/failed) | Monitored reopen lifecycle tracking |
| `djinn_arbiter_termination_total` | counter | `class` (infra/decision_failure) | Decision-vs-infra termination accounting |
| `djinn_arbiter_time_in_arbitration_seconds` | histogram | — | Time-in-arbitration distribution |

**Telemetry render tests:** `arbiter_decision_metric_names_and_labels_render` (line 2731), `arbiter_park_metric_names_and_labels_render` (line 2747), `arbiter_monitored_reopen_metric_names_and_labels_render` (line 2792), `arbiter_termination_metric_names_and_labels_render` (line 2811), `arbiter_time_in_arbitration_histogram_renders` (line 2826), `arbiter_metrics_do_not_contain_high_cardinality_labels` (line 2841).

---

## Hold-Cycle Reset Verification

**Status:** ✅ Code/Test

The coordinator and arbitration repository support hold-cycle archiving and fresh arbitration on release:

| Evidence | Path |
|----------|------|
| Hold release yields fresh arbitration | `server/crates/djinn-coordinator/src/tests/intervention.rs` — `hold_release_yields_fresh_arbitration_on_next_strike` (line 6359) |
| Hold release lifecycle | `server/crates/djinn-coordinator/src/tests/intervention.rs` — `review_hold_release_lifecycle_proves_dispatch_readiness_recovery` (line 2138) |
| Cycle advancement test | `server/crates/djinn-coordinator/src/tests/intervention.rs` — `consumed_arbitration_advances_to_next_cycle_and_dispatches` (line 3571) |

**Interpretation:** When a HumanReview hold is released/resolved, the current arbitration row is archived (consumed), and the next strike for that task gets a fresh `(task_id, hold_cycle + 1)` arbitration row with one new arbiter dispatch. Post-release strikes receive exactly one fresh arbitration per hold cycle.

---

## Operator-Only Rollout Observations 🔶

The following items cannot be verified by a worker running code/tests. They require live production observation after deployment.

### O1 — Two-week production metrics review

**What to observe:**
- `djinn_arbiter_decision_total` — decision distribution (approve vs reopen vs park ratio)
- `djinn_arbiter_park_total` — park rate before/after arbiter rollout
- `djinn_arbiter_time_in_arbitration_seconds` — P50/P95 time in arbitration
- `djinn_arbiter_termination_total` — infra vs decision-failure ratio
- `djinn_arbiter_monitored_reopen_total` — reopen success rate (started vs completed)

**Operator checklist:**
- [ ] Decision distribution shows no pathological park-only or approve-only skew after 2 weeks
- [ ] Park rate is stable or declining compared to pre-arbiter baseline
- [ ] P95 time-in-arbitration is within the configured deadline
- [ ] Infra termination class is not dominant (would indicate infra instability, not arbiter issues)
- [ ] Monitored reopen `started` count is close to `completed` count (no orphaned reopens)

### O2 — Legacy `request_lead` drain window

**What to observe:**
- After deployment, monitor for `deprecated_request_lead` activity events
- After 2+ production cycles with zero `deprecated_request_lead` events, the drain window is effectively closed
- The `tool_request_lead()` function in `djinn-mcp-extension/src/tool_defs.rs` (line 42) can be removed once the drain window closes

**Operator checklist:**
- [ ] Zero `deprecated_request_lead` activity events in the last 14 days
- [ ] No production errors related to the `request_lead` compatibility path
- [ ] Decide: remove `[HISTORICAL-COMPAT]` `tool_request_lead()` definition or extend drain window

---

## Operational Checklist 📋

For any operational rollout, confirm the following before enabling arbiter dispatch in production:

- [ ] Prometheus scrape is collecting the `djinn_arbiter_*` metric family
- [ ] Dashboard panels exist for decision distribution, park rate, time-in-arbitration, and termination class
- [ ] Alert thresholds configured for anomalous park rate spike or deadline-expired auto-parks
- [ ] `task_arbitrations` table has appropriate retention/TTL policy (arbitration rows accumulate per hold cycle)
- [ ] Lead agent sessions are configured to use the plan/strongest model pool
- [ ] Coordinator `enforce_expired_arbiter_deadline_before_dispatch` path is enabled (not behind a feature flag)

---

## Remaining Risk Assessment

### No remaining implementation gaps

All 13 acceptance criteria are mapped to landed code and tests. No criterion relies on unverifiable operator-only claims. The codebase evidence is complete at the time of this audit.

### Risk items for the next Planner

| Risk | Severity | Mitigation |
|------|----------|------------|
| `tool_request_lead()` historical-compat definition still exists in `djinn-mcp-extension/src/tool_defs.rs` | Low | Guarded by `[HISTORICAL-COMPAT]` comment and grep regression tests. Can be removed after drain window closes. Operator O2 checklist above. |
| No production dashboard/alerting for arbiter metrics yet | Medium | Metrics are landed and render correctly in tests. Operator O1 checklist above covers dashboard/alert setup as a rollout prerequisite. |
| `request_lead` references in stage.rs deprecated handler | Low | Handler emits `deprecated_request_lead` and routes to Planner, NOT to `needs_lead_intervention`. Guarded by `worker_deprecated_request_lead_does_not_produce_needs_lead_intervention` test. |
| HumanReview hold TTL/retention for `task_arbitrations` rows | Low | No retention policy set in migrations. Operational checklist item above. |

### Conclusion

**No additional implementation epics are needed before proposal `cxvq` can be completed.** All acceptance criteria are satisfied by landed code/tests from epics `7f8u`, `oelp`, `10qg`, and `aizl`. The remaining items are operational (dashboard/alert setup, drain window monitoring, retention policy) and are documented in the operator checklists above. A Planner can safely mark `cxvq` as complete once this artifact is reviewed.

---

## Source Epics and Delivering Tasks

| Epic | Status | Key Deliverables |
|------|--------|-----------------|
| `7f8u` — Arbiter foundation | ✅ Closed | Durable arbitration schema/repo (`enhd`), state-machine entry (`5z04`), atomic dispatch (`cmat`), re-entry dossiers (`0bnl`), recovery tests (`lvm4`) |
| `oelp` — Decision contract | ✅ Closed | `submit_decision` schema/validation (`ihzi`), park transaction + dossier (`q3uc`), approval gate (`k9hj`), monitored reopen (`zkk9`), termination accounting + deadline auto-park (`q8r6`) |
| `10qg` — Lead cut-over | ✅ Closed | `request_planner` routing + deprecated drain (`jurr`), `request_lead` removal (`8qyt`), forensic arbiter prompt + plan-lane (`htqo`), grep guards + regressions (`g9hd`) |
| `aizl` — Rollout hardening | ✅ (tasks closed) | Metrics + git-evidence payloads (`qk8b`), coordinator lifecycle regressions (`vi8e`), supervisor submit_decision regressions (`mshn`), legacy audit (`m2e8`), this artifact (`q1ka`) |

---

## Roadmap References

- `design/aizl-roadmap` — Epic aizl rollout hardening roadmap
- `design/7f8u-roadmap` — Epic 7f8u arbiter foundation roadmap
- `design/oelp-roadmap` — Epic oelp decision contract roadmap
- `design/10qg-roadmap` — Epic 10qg Lead cut-over roadmap
