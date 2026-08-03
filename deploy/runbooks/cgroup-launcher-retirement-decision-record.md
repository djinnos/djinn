# cgroup-launcher retirement decision record

`CGROUP_RETIREMENT_RECORD_SCHEMA: cgroup-launcher-retirement-decision-record/v1`

Copy this template into the change record for every decision attempt. Fill every
field with immutable references or `KEEP`; do not delete fields. This record is
the checklist consumed by the landing decision, not evidence that an operator
action happened.

## Identity and state

- `decision_id:`
- `recorded_at:`
- `operator:`
- `PREP_BASE:`
- `PREP_HEAD:`
- `RETIRE_BASE:`
- `RETIRE_HEAD:`
- `M:` (exact 40-hex landing commit; required for RETIRE)
- `decision_state:` `KEEP | RECOVERY | RETIRE`
- `decision_reason:`

`CGROUP_RETIREMENT_RECORD_KEEP_DEFAULT: failed, refused, skipped, inconclusive, stale, missing-owner, rejected, and bypassed pre-landing prerequisites are KEEP.`

`CGROUP_RETIREMENT_RECORD_RETIRE_COMPLETE_M: RETIRE requires a complete --landing M record.`

## Repository-verifiable preparation checklist

Attach immutable output or a content-addressed reference for each item.

- `prep_range_command:` `scripts/check-cgroup-retirement-gate.sh --prep PREP_BASE PREP_HEAD`
- `prep_range_result:` `green | failed | refused | skipped | inconclusive | stale | missing | rejected | bypassed`
- `asset_manifest:` `scripts/cgroup-retirement-assets.json`
- `asset_manifest_result:`
- `candidate_evidence_command:` `scripts/verify-cgroup-retirement-evidence.sh --candidate RETIRE_HEAD`
- `candidate_evidence_result:`
- `candidate_gate_command:` `scripts/check-cgroup-retirement-gate.sh --deploy --candidate RETIRE_HEAD --inputs <mandatory-inputs.json>`
- `candidate_gate_result:`
- `rollback_rehearsal_command:` `scripts/rehearse-cgroup-retirement-rollback.sh`
- `rollback_rehearsal_result:`
- `rollback_tree_identity_to_RETIRE_BASE:`
- `rollback_node_asset_restoration:`
- `rollback_launcher_leaf_fixture:`

## Operator-only evidence checklist

`CGROUP_RETIREMENT_RECORD_OPERATOR_ONLY: these fields require operator capture or human/hosting review. Repository tests and workers must leave them as recorded evidence, never as completed proof.`

- `production_class_capture_operator:`
- `five_canaries_and_final_run:`
- `zero_memory_events_oom_kill_delta:`
- `headroom_reservation_node_fit_kueue_width:`
- `live_RETIRE_CANARY_rehearsal_operator:`
- `deployment_observation_operator:`
- `live_launcher_leaf_evidence:`
- `required_human_approval_operator:`
- `effective_required_approvals:`
- `configured_owner_coverage:`
- `approved_current_reviewed_head:`
- `pull_request_identity:`
- `no_bypass_or_direct_push:`

## Commit-bound landing evidence for M

Every field below must bind to the same `M`.

- `landing_verifier_command:` `scripts/verify-cgroup-retirement-evidence.sh --landing M`
- `landing_verifier_result:`
- `image_oci_revision_M:`
- `image_digest_matches_expected:`
- `render_digest_M:`
- `node_digest_M:`
- `workload_digest_M:`
- `pod_annotation_M:` `djinn.dev/revision`
- `final_one_container_dispatch_confirmation:`
- `review_payload_child_seccomp_boundary:` `lost-complete`
- `review_payload_launcher_uid_separation:` `lost`
- `review_payload_second_in_worker_seccomp_installer:` `not-claimed`
- `review_payload_untested_replacements:` `[]`

## Fault and RECOVERY ledger

- `candidate_fault:` `true | false`
- `post_deploy_fault:` `true | false`
- `dispatch_state:` `paused | unchanged`
- `aggregate_tree_byte_identity:` `green | missing | failed`
- `node_asset_restoration:` `green | missing | failed`
- `live_launcher_leaf_restoration:` `green | missing | failed`
- `recovery_snapshot_references:`
- `rearm_runbook:` `deploy/runbooks/cgroup-launcher-rearm.md`

`CGROUP_RETIREMENT_RECORD_RECOVERY_EXCLUSIVE: candidate_fault or post_deploy_fault permits only RECOVERY until aggregate_tree_byte_identity, node_asset_restoration, and live_launcher_leaf_restoration are green.`

`CGROUP_RETIREMENT_RECORD_NO_RELABEL: incomplete RECOVERY must not be relabeled KEEP or RETIRE.`

## Terminal attestation

- `terminal_state:` `KEEP | RECOVERY | RETIRE`
- `terminal_attestor:`
- `terminal_timestamp:`
- `loss_launcher_uid_separation:` `lost` (required for RETIRE)
- `loss_child_seccomp_boundary:` `lost-complete` (required for RETIRE)
- `loss_second_in_worker_seccomp_installer:` `not-claimed` (required for RETIRE)

`CGROUP_RETIREMENT_RECORD_KEEP_NO_DELETION: KEEP lands no deletion commits and preserves launcher render, RuntimeClass, node assets, 3i92 retention gate, leaf creation, and lifting.`

`CGROUP_RETIREMENT_RECORD_RETIRE_ONLY_AFTER_LANDING: RETIRE is the only deletion authorization and only after the complete --landing M record succeeds.`
