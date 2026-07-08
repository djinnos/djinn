# Failover Observability Rollout Validation — Seven-Day Report & Query Artifact

> Epic: **wbn7** — Seven-day routing/failover observability and rollout validation
> Proposal: **uk2d** — Worker model routing + failover hardening: demote flaky models, fail over fast, stop discarding sessions
> Status: **Template — ready for operator execution after rollout apply**

---

## 1. Purpose

This document is the **operator-facing validation artifact** for proposal `uk2d`
after the lane-demotion and failover-chain orchestration code has landed. It
provides:

- Concrete PromQL and structured-log query templates comparing a **seven-day
  post-rollout window** against the **prior baseline** (the seven days before
  rollout apply).
- Evidence checklists for each of the six AC17 measures.
- Sample-evidence sections for lane resolution ordering, typed failures,
  prompt-context child spans, infra deltas, and fallback rescue.
- Rollback triggers with quantitative thresholds.

This is a **code/docs artifact**, not a production credential or access request.
All operator-only production-read steps are explicitly marked. No worker task is
required to run the seven-day production read — this document is the checkable
deliverable operators execute after rollout.

### Prerequisites

1. The lane-demotion rollout has been applied using the runbook in
   [`lane-demotion-rollout.md`](lane-demotion-rollout.md) (same directory).
2. The code instrumentation from tasks `dt2a`, `jo51`, and `zkxu` has been
   deployed to production.
3. A Prometheus-compatible metrics backend and structured-log aggregation system
   (e.g. Loki, Elasticsearch) are available.

---

## 2. Metric and Log Name Reference

All metric and log names below are **final names from landed code**. Do not
invent names that disagree with the source.

### 2.1 Prometheus Metrics

| Metric Name | Type | Labels | Source Module |
|---|---|---|---|
| `djinn_failover_candidate_attempts_total` | Counter | `outcome`, `provider_id`, `model_id` | `djinn_telemetry::failover` |
| `djinn_failover_candidate_accepted_total` | Counter | `provider_id`, `model_id` | `djinn_telemetry::failover` |
| `djinn_failover_chain_exhausted_total` | Counter | `provider_id`, `model_id` | `djinn_telemetry::failover` |
| `djinn_failover_latency_seconds` | Histogram | *(none)* | `djinn_telemetry::failover` |
| `djinn_zero_output_stall_seconds` | Histogram | `timeout_source`, `failure_class`, `chain_exhausted` | `djinn_telemetry::liveness_metrics` |
| `djinn_prompt_context_latency_seconds` | Histogram | *(none)* | `djinn_telemetry::prompt_context_metrics` |
| `djinn_prompt_context_child_span_latency_seconds` | Histogram | `span` | `djinn_telemetry::prompt_context_metrics` |
| `djinn_infra_exempt_total` | Counter | `outcome`, `is_infra` | `djinn_telemetry::infra_delta` |
| `djinn_fallback_rescue_total` | Counter | *(none)* | `djinn_telemetry::fallback_rescue` |
| `djinn_reasoning_kill_total` | Counter | `model_context`, `failure_class`, `outcome` | `djinn_telemetry::reasoning_kill` |

#### Bounded Label Values

**`outcome` for `djinn_failover_candidate_attempts_total`:**
`breaker_open`, `at_capacity`, `error`

**`timeout_source` / `failure_class` for `djinn_zero_output_stall_seconds`:**
`first_call_hang`, `idle_stall`

**`chain_exhausted` for `djinn_zero_output_stall_seconds`:**
`true`, `false`

**`span` for `djinn_prompt_context_child_span_latency_seconds`:**
`activity_db`, `epic_context`, `knowledge_context`, `attempt_history`,
`code_graph`, `reviewer_diff`

**`outcome` for `djinn_infra_exempt_total`:**
`park`, `quality_strike`, `total`

**`is_infra` for `djinn_infra_exempt_total`:**
`true`, `false`

**`model_context` for `djinn_reasoning_kill_total`:**
`reasoning`, `non_reasoning`

**`failure_class` for `djinn_reasoning_kill_total`:**
`first_call_hang`, `idle_stall`

**`outcome` for `djinn_reasoning_kill_total`:**
`killed`, `rescued`, `typed_failure`

#### High-Cardinality Dimensions (Structured Logs Only)

The following dimensions are **intentionally excluded** from Prometheus labels to
control series cardinality. They are available in structured tracing fields and
log queries:

- `task_id` — in `djinn.dispatch.task_id` tracing span and structured log fields
- `session_id` — in tracing fields when an active session is available
- `candidate_index` — in structured log fields for per-candidate detail

### 2.2 Structured Log Events

| Event Name | Level | Fields | Source |
|---|---|---|---|
| `lane_resolution_candidate` | `info` | `task_id`, `role`, `tenant_id`, `candidate_index`, `provider_id`, `model_id`, `last_resort` | `lane_resolution_log::emit_lane_resolution_candidates` |
| `failover_candidate_attempt` | `warn` | `task_id`, `role`, `candidate_index`, `total_candidates`, `provider_id`, `model_id`, `outcome`, `session_id` | `lane_resolution_log::emit_failover_candidate_attempt` |
| `failover_candidate_accepted` | `info` | `task_id`, `role`, `candidate_index`, `total_candidates`, `skipped_count`, `provider_id`, `model_id`, `session_id` | `lane_resolution_log::emit_failover_candidate_accepted` |
| `failover_chain_exhausted` | `warn` | `task_id`, `role`, `total_candidates`, `observed_failures`, `provider_id`, `model_id`, `chain_exhausted`, `session_id` | `lane_resolution_log::emit_failover_chain_exhausted` |
| `djinn.session_recovery.stall_timeout` | varies | task_id, session_id, timeout/failure fields | `session_recovery.rs` |
| `djinn.session_recovery.kill_session` | span | task_id, session_id | `session_recovery.rs` |
| `djinn.session_recovery.zombie_reap` | span | task_id, session_id | `session_recovery.rs` |

---

## 3. Query Templates — Baseline vs Seven-Day Post-Rollout

Each section below provides a PromQL template and/or structured-log query
template for one of the six AC17 measures. Operators should run each query
twice:

- **Baseline window:** The seven calendar days before the rollout apply.
- **Post-rollout window:** The seven calendar days after the rollout apply.

Replace `<baseline>` and `<post_rollout>` in the examples with the actual
Prometheus range selectors (e.g. `[7d]` with the appropriate `start`/`end`
parameters in Grafana or `curl`).

### 3.1 Failover Latency

**Objective:** Failover-chain traversal latency should not increase after
rollout. The p95 of `djinn_failover_latency_seconds` should remain within the
pre-rollout p95 ± 20%.

#### PromQL — p95 Failover Latency

```promql
# Post-rollout p95 failover latency
histogram_quantile(0.95,
  rate(djinn_failover_latency_seconds_bucket[7d])
)

# Baseline p95 failover latency (run against the prior 7-day window)
histogram_quantile(0.95,
  rate(djinn_failover_latency_seconds_bucket[7d] offset 7d)
)
```

#### PromQL — Failover Latency Comparison (ratio)

```promql
# Ratio of post-rollout to baseline p95 — should be ≤ 1.2
histogram_quantile(0.95, rate(djinn_failover_latency_seconds_bucket[7d]))
/
histogram_quantile(0.95, rate(djinn_failover_latency_seconds_bucket[7d] offset 7d))
```

#### Failover Candidate Attempt Breakdown

```promql
# Per-outcome, per-candidate attempt rate
sum by (outcome, provider_id, model_id) (
  rate(djinn_failover_candidate_attempts_total[7d])
)
```

#### Structured Log Query — Failover Chain Drill-Down

```
# Loki/LogQL — find tasks where chain was exhausted
{event="failover_chain_exhausted"}
| json
| observed_failures > 0
```

### 3.2 Zero-Output Wall-Clock

**Objective:** Sessions experiencing zero-output stalls should not increase
beyond baseline rates. Median and p95 wall-clock stall durations should remain
stable.

#### PromQL — p50 and p95 Zero-Output Stall Duration

```promql
# Post-rollout p50 stall duration
histogram_quantile(0.50,
  rate(djinn_zero_output_stall_seconds_bucket[7d])
)

# Post-rollout p95 stall duration
histogram_quantile(0.95,
  rate(djinn_zero_output_stall_seconds_bucket[7d])
)

# Baseline p95 stall duration
histogram_quantile(0.95,
  rate(djinn_zero_output_stall_seconds_bucket[7d] offset 7d)
)
```

#### PromQL — Stall Rate by Timeout Source

```promql
# Stall count rate by timeout source (post-rollout vs baseline)
sum by (timeout_source) (
  rate(djinn_zero_output_stall_seconds_count[7d])
)

sum by (timeout_source) (
  rate(djinn_zero_output_stall_seconds_count[7d] offset 7d)
)
```

#### PromQL — Chain-Exhausted Stall Ratio

```promql
# Fraction of stalls that exhausted the failover chain
sum(rate(djinn_zero_output_stall_seconds_count{chain_exhausted="true"}[7d]))
/
sum(rate(djinn_zero_output_stall_seconds_count[7d]))
```

#### Structured Log Query — Zero-Output Decision Drill-Down

```
# LogQL — stall timeout events with session context
{event=~"djinn.session_recovery.stall_timeout|djinn.session_recovery.kill_session"}
| json
| timeout_source="first_call_hang"
```

### 3.3 Prompt-Context Assembly Latency

**Objective:** Prompt-context assembly latency (total and per-child-span)
should not regress beyond baseline p95 ± 15%. A regression indicates that
failover-related prompt plumbing added overhead.

#### PromQL — Total Prompt-Context Assembly p95

```promql
# Post-rollout p95 total assembly latency
histogram_quantile(0.95,
  rate(djinn_prompt_context_latency_seconds_bucket[7d])
)

# Baseline p95 total assembly latency
histogram_quantile(0.95,
  rate(djinn_prompt_context_latency_seconds_bucket[7d] offset 7d)
)
```

#### PromQL — Per Child-Span Latency Breakdown

```promql
# p95 latency by child-span phase (post-rollout)
histogram_quantile(0.95,
  rate(djinn_prompt_context_child_span_latency_seconds_bucket[7d])
) by (span)

# p95 latency by child-span phase (baseline)
histogram_quantile(0.95,
  rate(djinn_prompt_context_child_span_latency_seconds_bucket[7d] offset 7d)
) by (span)
```

#### PromQL — Child-Span Latency Comparison

```promql
# Ratio of post-rollout to baseline p95 per span — alert if > 1.15
histogram_quantile(0.95,
  rate(djinn_prompt_context_child_span_latency_seconds_bucket[7d])
)
/
histogram_quantile(0.95,
  rate(djinn_prompt_context_child_span_latency_seconds_bucket[7d] offset 7d)
)
```

### 3.4 Infra Park/Strike Deltas

**Objective:** After rollout, infra-classified failures (`timed_out`,
`spawn_failed`, `crashed`) should be excluded from quality-strike and park
escalation counters. The `djinn_infra_exempt_total` metric tracks this
separation. The delta between `is_infra="true"` and `is_infra="false"` for
`outcome="park"` should show infra-exempt attempts are not counted as
quality-strike-class.

#### PromQL — Park Outcome Delta (Infra vs Quality-Strike)

```promql
# Post-rollout: infra-exempt park events
sum(rate(djinn_infra_exempt_total{outcome="park", is_infra="true"}[7d]))

# Post-rollout: quality-strike park events
sum(rate(djinn_infra_exempt_total{outcome="park", is_infra="false"}[7d]))

# Baseline: total park events (pre-infra-exemption)
sum(rate(djinn_infra_exempt_total{outcome="park"}[7d] offset 7d))
```

#### PromQL — Quality Strike Rate (Infra Exempted)

```promql
# Quality-strike rate excluding infra
sum(rate(djinn_infra_exempt_total{outcome="quality_strike", is_infra="false"}[7d]))

# Total quality-strike rate (both classifications)
sum(rate(djinn_infra_exempt_total{outcome="quality_strike"}[7d]))
```

#### PromQL — Infra Exemption Ratio

```promql
# Fraction of total outcomes that are infra-exempt
sum(rate(djinn_infra_exempt_total{is_infra="true"}[7d]))
/
sum(rate(djinn_infra_exempt_total[7d]))
```

#### Reopen/Park Delta via Existing Metrics

```promql
# Park rate (djinn_tasks_parked_total) — should not increase due to infra failures
sum(rate(djinn_tasks_parked_total[7d]))

# Reopen rate (djinn_task_reopens_total) — should not spike
sum(rate(djinn_task_reopens_total[7d]))
```

### 3.5 Fallback Rescue Rate

**Objective:** Fallback rescue events (`djinn_fallback_rescue_total`) indicate
a later failover candidate accepted a dispatch after earlier candidates failed.
The rescue rate should be stable or increase slightly (indicating the failover
chain is working as designed). A rescue rate of zero combined with non-zero
chain exhaustion indicates the chain is failing without rescue.

#### PromQL — Fallback Rescue Rate

```promql
# Post-rollout rescue rate
rate(djinn_fallback_rescue_total[7d])

# Baseline rescue rate
rate(djinn_fallback_rescue_total[7d] offset 7d)
```

#### PromQL — Rescue-to-Exhaustion Ratio

```promql
# Ratio of rescues to chain exhaustions — should be > 0
# (a ratio of 0 means no rescues are happening)
rate(djinn_fallback_rescue_total[7d])
/
sum(rate(djinn_failover_chain_exhausted_total[7d]))
```

#### PromQL — Rescue vs First-Candidate Acceptance

```promql
# Rate of candidate acceptance by candidate model
sum by (provider_id, model_id) (
  rate(djinn_failover_candidate_accepted_total[7d])
)
```

### 3.6 Reasoning-Model False-Positive Kill Rate

**Objective:** Reasoning models (mimo, glm, o1, o3, *thinking) should not be
disproportionately killed by stall timeouts. The false-positive kill rate for
reasoning models should not exceed 2× the non-reasoning kill rate. The
`djinn_reasoning_kill_total` metric classifies outcomes by model context,
failure class, and outcome.

#### PromQL — Kill Rate by Model Context and Failure Class

```promql
# Post-rollout: killed outcomes by model context and failure class
sum by (model_context, failure_class) (
  rate(djinn_reasoning_kill_total{outcome="killed"}[7d])
)

# Baseline
sum by (model_context, failure_class) (
  rate(djinn_reasoning_kill_total{outcome="killed"}[7d] offset 7d)
)
```

#### PromQL — Reasoning vs Non-Reasoning Kill Ratio

```promql
# Kill rate ratio: reasoning / non_reasoning
# Alert if > 2.0
sum(rate(djinn_reasoning_kill_total{model_context="reasoning", outcome="killed"}[7d]))
/
sum(rate(djinn_reasoning_kill_total{model_context="non_reasoning", outcome="killed"}[7d]))
```

#### PromQL — Rescued Rate for Reasoning Models

```promql
# Rescued reasoning models — should be > 0 when kills happen
sum by (failure_class) (
  rate(djinn_reasoning_kill_total{model_context="reasoning", outcome="rescued"}[7d])
)
```

#### PromQL — Typed Failure Rate for Reasoning Models

```promql
# Typed failure rate for reasoning models
sum by (failure_class) (
  rate(djinn_reasoning_kill_total{model_context="reasoning", outcome="typed_failure"}[7d])
)
```

---

## 4. Evidence Checklists

Operators fill these checklists during the seven-day post-rollout observation
period. Each item requires a concrete evidence reference (metric screenshot,
log query result, or structured-log sample).

### 4.1 Failover Latency Evidence

- [ ] **p95 latency stable:** p95 of `djinn_failover_latency_seconds` within
  ±20% of baseline. Evidence: `<screenshot/query result>`.
- [ ] **No latency outliers:** No sustained p99 > 2× baseline p99. Evidence:
  `<screenshot/query result>`.
- [ ] **Candidate attempt distribution sane:** Attempt rates by outcome
  (`breaker_open`, `at_capacity`, `error`) not dominated by a single outcome.
  Evidence: `<query result>`.
- [ ] **Chain exhaustion rate stable:** `djinn_failover_chain_exhausted_total`
  rate within ±30% of baseline (some increase is acceptable if failover is
  exercising more candidates). Evidence: `<query result>`.

### 4.2 Zero-Output Wall-Clock Evidence

- [ ] **p95 stall duration stable:** p95 of `djinn_zero_output_stall_seconds`
  within ±20% of baseline. Evidence: `<screenshot/query result>`.
- [ ] **Stall rate not increasing:** Total stall event rate (per 7 days) not
  more than 1.5× baseline. Evidence: `<query result>`.
- [ ] **Timeout source distribution balanced:** `first_call_hang` and
  `idle_stall` proportions similar to baseline. Evidence: `<query result>`.
- [ ] **Chain-exhausted stall fraction reasonable:** Fraction of stalls
  where `chain_exhausted="true"` not exceeding 10% of total stalls. Evidence:
  `<query result>`.

### 4.3 Prompt-Context Assembly Latency Evidence

- [ ] **Total assembly p95 stable:** `djinn_prompt_context_latency_seconds`
  p95 within ±15% of baseline. Evidence: `<screenshot/query result>`.
- [ ] **No child-span regression:** Each child span (`activity_db`,
  `epic_context`, `knowledge_context`, `attempt_history`, `code_graph`,
  `reviewer_diff`) p95 within ±15% of baseline. Evidence: `<query result>`.
- [ ] **No new latency outlier spans:** No child span p99 > 3× its baseline
  p99. Evidence: `<query result>`.

### 4.4 Infra Park/Strike Delta Evidence

- [ ] **Infra-exempt parks counted separately:** `djinn_infra_exempt_total`
  with `outcome="park", is_infra="true"` shows non-zero counts for infra
  failures. Evidence: `<query result>`.
- [ ] **Quality-strike rate not inflated:** `djinn_infra_exempt_total` with
  `outcome="quality_strike", is_infra="false"` rate not higher than baseline
  total quality-strike rate. Evidence: `<query result>`.
- [ ] **Park escalation rate stable:** `djinn_tasks_parked_total` rate within
  ±20% of baseline. Evidence: `<query result>`.
- [ ] **Reopen rate stable:** `djinn_task_reopens_total` rate within ±20% of
  baseline. Evidence: `<query result>`.

### 4.5 Fallback Rescue Evidence

- [ ] **Rescue events occurring:** `djinn_fallback_rescue_total` rate > 0
  (rescue is functioning). Evidence: `<query result>`.
- [ ] **Rescue-to-exhaustion ratio healthy:** Ratio of rescue rate to chain
  exhaustion rate > 0.5 (rescues are more common than full exhaustions).
  Evidence: `<query result>`.
- [ ] **Rescued sessions not quality-struck:** No `djinn_tasks_parked_total`
  increments correlated with rescue events (confirming the ry9v guarantee).
  Evidence: `<correlation query or manual review>`.

### 4.6 Reasoning False-Positive Kill Evidence

- [ ] **Reasoning kill rate not disproportionate:** Kill rate ratio
  (reasoning / non_reasoning) < 2.0. Evidence: `<query result>`.
- [ ] **Reasoning kills rescued when possible:** `djinn_reasoning_kill_total`
  with `model_context="reasoning", outcome="rescued"` > 0 when kills occur.
  Evidence: `<query result>`.
- [ ] **Typed failures classified:** `djinn_reasoning_kill_total` with
  `outcome="typed_failure"` is populated (showing the system classifies rather
  than blanket-kills). Evidence: `<query result>`.
- [ ] **First-call-hang not dominant for reasoning:** `failure_class`
  distribution for reasoning kills not overwhelmingly `first_call_hang` (>80%
  would indicate backend latency masquerading as hangs). Evidence: `<query
  result>`.

---

## 5. Lane-Resolution Sample Guidance

This section documents how to inspect lane-resolution candidate ordering using
structured logs, for both post-apply and post-rollback verification.

### 5.1 Post-Apply Candidate Order Verification

After applying the lane-demotion rollout (see
[`lane-demotion-rollout.md`](lane-demotion-rollout.md)), verify that the
candidate order matches the expected payload:

**Expected implement lane order (post-apply):**

| Index | Provider/Model | `last_resort` |
|---|---|---|
| 0 | `xiaomi-token-plan-sgp/mimo-v2.5-pro` | `false` |
| 1 | `zai-coding-plan/glm-5.2` | `false` |
| 2 | `kimi-for-coding/k2p7` | `true` |
| 3 | `minimax-coding-plan/MiniMax-M3` | `true` |

**Structured log query (Loki/LogQL):**

```
# Lane resolution candidates for implement role after rollout apply
{event="lane_resolution_candidate"}
| json
| role="implement"
| line_format "{{.candidate_index}} {{.provider_id}}/{{.model_id}} last_resort={{.last_resort}}"
```

**Verification:**
- [ ] Index 0 shows `xiaomi-token-plan-sgp/mimo-v2.5-pro` with `last_resort=false`.
- [ ] Index 1 shows `zai-coding-plan/glm-5.2` with `last_resort=false`.
- [ ] Index 2 shows `kimi-for-coding/k2p7` with `last_resort=true`.
- [ ] Index 3 shows `minimax-coding-plan/MiniMax-M3` with `last_resort=true`.

**Expected review lane order:** Same as implement (see §2.2 of
`lane-demotion-rollout.md`).

```
# Lane resolution candidates for review role
{event="lane_resolution_candidate"}
| json
| role="review"
| line_format "{{.candidate_index}} {{.provider_id}}/{{.model_id}} last_resort={{.last_resort}}"
```

### 5.2 Post-Rollback Candidate Order Verification

After rollback (restoring the pre-rollout snapshot), verify the original
candidate order is restored:

```
# Post-rollback: verify original lane order is restored
{event="lane_resolution_candidate"}
| json
| role="implement"
| line_format "{{.candidate_index}} {{.provider_id}}/{{.model_id}} last_resort={{.last_resort}}"
```

- [ ] Candidate order matches the pre-snapshot `model_lanes` JSON from the
  rollback artifact (`fixtures/pre-rollout-snapshot.json`).

### 5.3 Failover Candidate Sample Inspection

To inspect individual failover-chain traversals:

```
# All failover candidate events for a specific task
{event=~"failover_candidate_attempt|failover_candidate_accepted|failover_chain_exhausted"}
| json
| task_id="<task_id>"
| line_format "{{.event}} index={{.candidate_index}}/{{.total_candidates}} {{.provider_id}}/{{.model_id}} outcome={{.outcome}} skipped={{.skipped_count}}"
```

**Sample fields per event:**

| Event | Key Fields | Meaning |
|---|---|---|
| `failover_candidate_attempt` | `outcome` (breaker_open/at_capacity/error), `candidate_index`, `total_candidates` | A candidate was tried and failed |
| `failover_candidate_accepted` | `candidate_index`, `skipped_count`, `total_candidates` | A candidate accepted the dispatch |
| `failover_chain_exhausted` | `observed_failures`, `total_candidates`, `chain_exhausted=true` | All candidates were tried and none accepted |

---

## 6. Typed Failure and Prompt-Context Sample Evidence

### 6.1 Typed Failure Samples

Typed failures are classified by the `failure_class` label on
`djinn_reasoning_kill_total` and `djinn_zero_output_stall_seconds`. Operators
should collect representative samples:

```
# Reasoning kill events with typed failure classification
{event=~"failover_candidate_attempt|djinn.session_recovery.kill_session"}
| json
| model_id=~".*mimo.*|.*glm.*"
| line_format "task={{.task_id}} model={{.model_id}} outcome={{.outcome}} failure_class={{.failure_class}}"
```

**Sample evidence template:**

| Task ID | Model | Failure Class | Outcome | Notes |
|---|---|---|---|---|
| `<task_id>` | `<model_id>` | `<class>` | `<outcome>` | `<notes>` |

- [ ] At least 3 typed failure samples collected across different failure classes.
- [ ] No single failure class accounts for >80% of reasoning-model outcomes.

### 6.2 Prompt-Context Child-Span Latency Samples

Collect latency samples from the per-child-span histogram to identify outliers:

```promql
# Child-span p50, p95, p99 breakdown
histogram_quantile(0.50, rate(djinn_prompt_context_child_span_latency_seconds_bucket[1h])) by (span)
histogram_quantile(0.95, rate(djinn_prompt_context_child_span_latency_seconds_bucket[1h])) by (span)
histogram_quantile(0.99, rate(djinn_prompt_context_child_span_latency_seconds_bucket[1h])) by (span)
```

**Sample evidence template:**

| Span | p50 (ms) | p95 (ms) | p99 (ms) | Baseline p95 (ms) | Ratio |
|---|---|---|---|---|---|
| `activity_db` | | | | | |
| `epic_context` | | | | | |
| `knowledge_context` | | | | | |
| `attempt_history` | | | | | |
| `code_graph` | | | | | |
| `reviewer_diff` | | | | | |

- [ ] All child-span p95 values within ±15% of baseline.
- [ ] No child-span p99 > 3× its baseline p99.

### 6.3 Infra Reopen/Park Delta Samples

Collect delta samples showing infra-exempt vs quality-strike outcomes:

```
# Infra exemption events
{event=~"djinn.session_recovery.stall_timeout|djinn.session_recovery.zombie_reap"}
| json
| line_format "task={{.task_id}} session={{.session_id}} timeout_source={{.timeout_source}} failure_class={{.failure_class}}"
```

**Sample evidence template:**

| Metric | Post-Rollout Value | Baseline Value | Delta |
|---|---|---|---|
| `djinn_infra_exempt_total{outcome="park", is_infra="true"}` (7d rate) | | | |
| `djinn_infra_exempt_total{outcome="park", is_infra="false"}` (7d rate) | | | |
| `djinn_infra_exempt_total{outcome="quality_strike", is_infra="true"}` (7d rate) | | | |
| `djinn_infra_exempt_total{outcome="quality_strike", is_infra="false"}` (7d rate) | | | |
| `djinn_tasks_parked_total` (7d rate) | | | |
| `djinn_task_reopens_total` (7d rate) | | | |

- [ ] Infra-exempt park events present and non-zero.
- [ ] Quality-strike rate not inflated compared to baseline.

### 6.4 Fallback Rescue Evidence

**Sample evidence template:**

| Metric | Post-Rollout Value | Baseline Value |
|---|---|---|
| `djinn_fallback_rescue_total` (7d sum) | | |
| `djinn_failover_chain_exhausted_total` (7d sum) | | |
| Rescue-to-exhaustion ratio | | |

**Sample rescue event log:**

```
# Recent rescue events (candidate_index > 0 accepted)
{event="failover_candidate_accepted"}
| json
| candidate_index > 0
| line_format "task={{.task_id}} rescued_by={{.provider_id}}/{{.model_id}} at_index={{.candidate_index}} skipped={{.skipped_count}}"
```

- [ ] Rescue events present with `candidate_index > 0`.
- [ ] Rescued sessions show no corresponding `djinn_tasks_parked_total`
  increment (ry9v guarantee preserved).

---

## 7. Rollback Triggers

Execute rollback if **any** of the following conditions persist for more than
24 hours during the seven-day observation window. Use the rollback procedure in
[`lane-demotion-rollout.md`](lane-demotion-rollout.md) §5.

### 7.1 mimo/glm Capacity Regression

**Trigger:** The primary model (`xiaomi-token-plan-sgp/mimo-v2.5-pro`) or
secondary model (`zai-coding-plan/glm-5.2`) shows a sustained capacity
degradation:

| Condition | Threshold | Metric |
|---|---|---|
| mimo breaker-open rate | > 50% of mimo dispatch attempts result in `breaker_open` | `djinn_failover_candidate_attempts_total{outcome="breaker_open", provider_id="xiaomi-token-plan-sgp", model_id="mimo-v2.5-pro"}` / `djinn_failover_candidate_attempts_total{provider_id="xiaomi-token-plan-sgp", model_id="mimo-v2.5-pro"}` |
| glm breaker-open rate | > 50% of glm dispatch attempts result in `breaker_open` | `djinn_failover_candidate_attempts_total{outcome="breaker_open", provider_id="zai-coding-plan", model_id="glm-5.2"}` / `djinn_failover_candidate_attempts_total{provider_id="zai-coding-plan", model_id="glm-5.2"}` |
| Chain exhaustion rate | > 3× baseline chain exhaustion rate | `djinn_failover_chain_exhausted_total` 7d rate vs baseline |
| mimo error rate | > 30% of mimo attempts result in `error` | `djinn_failover_candidate_attempts_total{outcome="error", provider_id="xiaomi-token-plan-sgp", model_id="mimo-v2.5-pro"}` / attempt total |

**PromQL — mimo breaker-open ratio (alert if > 0.5):**

```promql
sum(rate(djinn_failover_candidate_attempts_total{outcome="breaker_open", provider_id="xiaomi-token-plan-sgp", model_id="mimo-v2.5-pro"}[1h]))
/
sum(rate(djinn_failover_candidate_attempts_total{provider_id="xiaomi-token-sgp", model_id="mimo-v2.5-pro"}[1h]))
```

**Action:** Roll back lane order using the pre-apply snapshot per
`lane-demotion-rollout.md` §5.

### 7.2 Reasoning False-Positive Kill Regression

**Trigger:** Reasoning models are being killed at a disproportionate rate
compared to non-reasoning models:

| Condition | Threshold | Metric |
|---|---|---|
| Reasoning kill ratio | > 2.0× the non-reasoning kill rate, sustained for 24h | `djinn_reasoning_kill_total{model_context="reasoning", outcome="killed"}` / `djinn_reasoning_kill_total{model_context="non_reasoning", outcome="killed"}` |
| Reasoning rescue deficit | Reasoning models killed (`outcome="killed"`) with zero corresponding rescues (`outcome="rescued"`) over a 24h window | `djinn_reasoning_kill_total{model_context="reasoning", outcome="killed"}` > 0 AND `djinn_reasoning_kill_total{model_context="reasoning", outcome="rescued"}` == 0 |
| First-call-hang dominance | > 80% of reasoning kills are `failure_class="first_call_hang"` for 24h | Indicates backend latency being misclassified as hangs |

**PromQL — Reasoning kill ratio (alert if > 2.0):**

```promql
sum(rate(djinn_reasoning_kill_total{model_context="reasoning", outcome="killed"}[1h]))
/
sum(rate(djinn_reasoning_kill_total{model_context="non_reasoning", outcome="killed"}[1h]))
```

**PromQL — Reasoning rescue availability check:**

```promql
# Should return > 0 when reasoning kills are occurring
sum(rate(djinn_reasoning_kill_total{model_context="reasoning", outcome="rescued"}[24h]))
```

**Action:** Roll back lane order. If the false-positive kill regression is
specific to `first_call_hang` classification, consider tuning the stall
timeout threshold in `server/crates/djinn-core/src/liveness.rs` before
re-attempting rollout.

### 7.3 General Rollback Checklist

- [ ] Rollback trigger condition confirmed (sustained > 24 hours).
- [ ] Pre-apply snapshot loaded and verified (see
  `lane-demotion-rollout.md` §3).
- [ ] Org default lanes restored from snapshot.
- [ ] Per-user lanes restored from snapshot.
- [ ] Post-rollback verification: snapshot JSON == current DB JSON.
- [ ] Post-rollback dispatch logs reflect restored candidate order (§5.2).
- [ ] Rollback reason documented with specific metric/query evidence.

---

## 8. Acceptance Criteria Mapping

| AC17 Measure | Query Template Section | Evidence Checklist Section |
|---|---|---|
| Failover latency | §3.1 | §4.1 |
| Zero-output wall-clock | §3.2 | §4.2 |
| Prompt-context assembly latency | §3.3 | §4.3 |
| Infra park/strike deltas | §3.4 | §4.4 |
| Fallback rescue rate | §3.5 | §4.5 |
| Reasoning false-positive kill rate | §3.6 | §4.6 |
| Lane-resolution ordering | §5 | §5.1, §5.2 |
| Rollback triggers | §7 | §7.3 |

---

## 9. References

- [`lane-demotion-rollout.md`](lane-demotion-rollout.md) — Default lane
  demotion rollout runbook (apply, snapshot, rollback, round-trip proof).
- `server/crates/djinn-telemetry/src/lib.rs` — Prometheus metric definitions
  and registration (failover, liveness, prompt-context, infra-delta,
  fallback-rescue, reasoning-kill modules).
- `server/crates/djinn-coordinator/src/dispatch/lane_resolution_log.rs` —
  Structured log events for lane-resolution candidates and failover traversal.
- `server/crates/djinn-coordinator/src/dispatch/session_recovery.rs` —
  Session stall/kill/reap decision logic and telemetry emission points.
- `server/crates/djinn-coordinator/src/dispatch/task_dispatch.rs` —
  Failover-chain traversal, dispatch outcomes, and fallback-rescue logic.
- `server/crates/djinn-agent/src/actors/slot/lifecycle/prompt_context.rs` —
  Prompt-context assembly with child-span latency recording.
- `server/crates/djinn-core/src/liveness.rs` — Zero-token/zero-output stall
  detection thresholds.
- Memory note: `design/wbn7-roadmap` — Epic roadmap and wave plan.
- Memory note: `design/working-spec-1etf` — Working spec for planning task.
