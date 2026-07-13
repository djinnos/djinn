# Tool-call telemetry decision record — blank template

Copy this template, fill in all fields, and commit it under
`docs/telemetry/decisions/YYYY-MM-DD_codex_surface_0tyy.md` (or your project's
chosen path). Do not leave any required field blank. Synthetic example values are
available in `docs/telemetry-decision-record-example.md`.

## Window

- `start_day`: ____________________
- `end_day`: ____________________
- `source_description`: ____________________
- `operator_who_ran_export`: ____________________
- `export_timestamp_utc`: ____________________

## Source completeness

- `transcript_authority`: persisted `session_messages` rows
- `trace_enrichment`: Langfuse / OTel / none
- `enrichment_fields_added`: ____________________
- `sessions_considered`: ____________________
- `sessions_excluded_reason`: ____________________

## Missing required fields

List any missing required fields after running the evaluator with a placeholder
audit. A non-empty list here makes the final decision `insufficient data`.

- `missing_required_fields`: ____________________
- `remediation_action`: ____________________

## Populations

### Candidate (Codex / OpenAI Responses surface)

- `definition`: ____________________
- `row_count`: ____________________
- `distinct_sessions`: ____________________
- `distinct_tasks`: ____________________
- `distinct_roles`: ____________________
- `edit_calls`: ____________________
- `apply_patch_calls`: ____________________
- `modifying_sessions`: ____________________
- `edit_failure_rate`: ____________________
- `apply_patch_failure_rate`: ____________________
- `retry_after_edit_failure_rate`: ____________________

### Baseline (matched default / Responses/default surface)

- `definition`: ____________________
- `row_count`: ____________________
- `distinct_sessions`: ____________________
- `distinct_tasks`: ____________________
- `distinct_roles`: ____________________
- `edit_calls`: ____________________
- `apply_patch_calls`: ____________________
- `modifying_sessions`: ____________________
- `edit_failure_rate`: ____________________
- `apply_patch_failure_rate`: ____________________
- `retry_after_edit_failure_rate`: ____________________

## Sample minima

- `edit_calls_minimum`: ____________________
- `apply_patch_calls_minimum`: ____________________
- `modifying_sessions_minimum`: ____________________
- `tasks_minimum`: ____________________
- `roles_minimum`: ____________________
- `sample_minima_shortfalls`: ____________________

## Pseudo-count ratio

- `candidate_edit_failures_with_pseudo_count`: ____________________
- `baseline_edit_failures_with_pseudo_count`: ____________________
- `pseudo_count_ratio`: ____________________
- `pseudo_count_ratio_threshold`: ____________________
- `passed`: ____________________

## Wilson interval

- `95_percent_wilson_difference_interval_lower`: ____________________
- `95_percent_wilson_difference_interval_upper`: ____________________
- `excludes_zero`: ____________________

## Threshold outcomes

| Gate | Threshold | Observed | Passed |
|------|-----------|----------|--------|
| `edit_disadvantage` | >= ___ pp | ___ pp | ___ |
| `pseudo_count_ratio` | >= ___ | ___ | ___ |
| `retry_disadvantage` | >= ___ pp | ___ pp | ___ |
| `wilson_difference_excludes_zero` | 95% CI excludes zero | ___ | ___ |
| `manual_audit` | ___ of ___ qualifying | ___ of ___ | ___ |

## 20-trace audit result

- `sampled`: ____________________
- `qualifying`: ____________________
- `completed`: ____________________
- `sampler_method`: ____________________
- `audit_completed_by`: ____________________
- `classification_notes`: ____________________

## Final decision

- `decision`: GO / STOP / insufficient data
- `rationale`: ____________________
- `blocking_gates_or_shortfalls`: ____________________

## Production-only attestations

Check only the boxes that are true:

- [ ] Production data collection was performed by an operator, not by CI.
- [ ] The 20-trace manual audit was performed by a human reviewer, not by CI.
- [ ] No missing required fields remain.
- [ ] All sample minima are satisfied.
- [ ] All threshold outcomes are recorded above.

## CI-produced synthetic evidence

- [ ] Deterministic end-to-end synthetic fixtures passed in CI.
- [ ] Model-facing surface snapshot guard passed in CI.
- [ ] CI did **not** perform production collection or manual audit.

## Sign-off

- `record_written_by`: ____________________
- `reviewed_by`: ____________________
- `date`: ____________________
