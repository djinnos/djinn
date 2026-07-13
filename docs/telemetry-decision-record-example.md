# Tool-call telemetry decision record — synthetic example

This is a **synthetic example** for documentation and template validation. It
uses fabricated counts and does **not** represent production evidence. CI uses
deterministic synthetic fixtures that exercise the same pipeline but with much
smaller, machine-generated transcripts; they do not assert that production
collection or a manual audit occurred.

## Window

- `start_day`: 2026-07-01
- `end_day`: 2026-07-30
- `source_description`: Synthetic persisted session transcripts for template validation
- `operator_who_ran_export`: N/A — synthetic example
- `export_timestamp_utc`: 2026-07-31T00:00:00Z

## Source completeness

- `transcript_authority`: synthetic `PersistedTranscript` fixtures
- `trace_enrichment`: none
- `enrichment_fields_added`: none
- `sessions_considered`: 40
- `sessions_excluded_reason`: 0

## Missing required fields

- `missing_required_fields`: (none)
- `remediation_action`: N/A — synthetic data is complete by construction

## Populations

### Candidate (Codex / OpenAI Responses surface)

- `definition`: OpenAIResponses + codex surface
- `row_count`: 1000
- `distinct_sessions`: 40
- `distinct_tasks`: 10
- `distinct_roles`: 2
- `edit_calls`: 500
- `apply_patch_calls`: 500
- `modifying_sessions`: 40
- `edit_failure_rate`: 80 / 500 = 0.160
- `apply_patch_failure_rate`: 10 / 500 = 0.020
- `retry_after_edit_failure_rate`: 120 / 160 = 0.750

### Baseline (matched default / Responses/default surface)

- `definition`: matched default/Responses baseline
- `row_count`: 1000
- `distinct_sessions`: 40
- `distinct_tasks`: 10
- `distinct_roles`: 2
- `edit_calls`: 500
- `apply_patch_calls`: 500
- `modifying_sessions`: 40
- `edit_failure_rate`: 10 / 500 = 0.020
- `apply_patch_failure_rate`: 10 / 500 = 0.020
- `retry_after_edit_failure_rate`: 20 / 160 = 0.125

## Sample minima

- `edit_calls_minimum`: 100
- `apply_patch_calls_minimum`: 100
- `modifying_sessions_minimum`: 30
- `tasks_minimum`: 5
- `roles_minimum`: 2
- `sample_minima_shortfalls`: (none)

## Pseudo-count ratio

- `candidate_edit_failures_with_pseudo_count`: 81 / 501 = 0.162
- `baseline_edit_failures_with_pseudo_count`: 11 / 501 = 0.022
- `pseudo_count_ratio`: 7.36
- `pseudo_count_ratio_threshold`: 1.5
- `passed`: true

## Wilson interval

- `95_percent_wilson_difference_interval_lower`: 0.110
- `95_percent_wilson_difference_interval_upper`: 0.190
- `excludes_zero`: true

## Threshold outcomes

| Gate | Threshold | Observed | Passed |
|------|-----------|----------|--------|
| `edit_disadvantage` | >= 5.0 pp | 14.0 pp | true |
| `pseudo_count_ratio` | >= 1.5 | 7.36 | true |
| `retry_disadvantage` | >= 10.0 pp | 62.5 pp | true |
| `wilson_difference_excludes_zero` | 95% CI excludes zero | yes | true |
| `manual_audit` | 12 of 20 qualifying | 12 of 20 | true |

## 20-trace audit result

- `sampled`: 20
- `qualifying`: 12
- `completed`: true
- `sampler_method`: stratified random sample across failed edit sessions
- `audit_completed_by`: synthetic reviewer
- `classification_notes`: Example only. Genuine classifications require a human reviewer.

## Final decision

- `decision`: GO
- `rationale`: All required fields present, all sample minima satisfied, all four quantitative gates passed, and the 20-trace audit reached 12 qualifying cases. This is synthetic data; the same pipeline would record STOP or insufficient data if any gate failed or data were incomplete.
- `blocking_gates_or_shortfalls`: (none)

## Production-only attestations

- [ ] Production data collection was performed by an operator, not by CI.
- [ ] The 20-trace manual audit was performed by a human reviewer, not by CI.
- [ ] No missing required fields remain.
- [ ] All sample minima are satisfied.
- [ ] All threshold outcomes are recorded above.

## CI-produced synthetic evidence

- [x] Deterministic end-to-end synthetic fixtures passed in CI.
- [x] Model-facing surface snapshot guard passed in CI.
- [x] CI did **not** perform production collection or manual audit.

## Sign-off

- `record_written_by`: synthetic example
- `reviewed_by`: synthetic example
- `date`: 2026-07-31

---

**Note:** This example is intentionally labeled synthetic. The fixtures in the
repository are also synthetic. They demonstrate that the pipeline correctly
reaches GO, STOP, and `insufficient data` outcomes without representing the
synthetic data as production evidence.
