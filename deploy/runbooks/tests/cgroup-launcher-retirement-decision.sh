#!/usr/bin/env bash
# Hermetic static contract for the cgroup-launcher retirement decision runbook.
# It reads repository markdown only. A pass never captures production evidence,
# rehearses RETIRE_CANARY, observes a deployment, or supplies human approval.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUNBOOK="$SCRIPT_DIR/../cgroup-launcher-retirement-decision.md"
RECORD="$SCRIPT_DIR/../cgroup-launcher-retirement-decision-record.md"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() { printf 'FAIL: %s\n' "$*" >&2; return 1; }
require() { local f=$1 d=$2 p=$3; grep -Fq -- "$p" "$f" || { fail "missing $d"; return 1; }; }
line() { grep -nF -- "$2" "$1" | head -n1 | cut -d: -f1; }
before() {
  local f=$1 d=$2 a b; a=$(line "$f" "$3"); b=$(line "$f" "$4")
  [[ -n "$a" && -n "$b" && "$a" -lt "$b" ]] || { fail "$d"; return 1; }
}

validate() {
  local runbook=$1 record=$2
  [[ -s "$runbook" && -s "$record" ]] || { fail 'runbook or decision record missing'; return 1; }

  # The scope and operator-only boundary keep this documentation from turning a
  # repository fixture into a deletion authorization.
  require "$runbook" 'decision scope' 'CGROUP_RETIREMENT_DECISION_SCOPE:' || return 1
  require "$runbook" 'operator-only boundary' 'CGROUP_RETIREMENT_OPERATOR_ONLY_BOUNDARY:' || return 1
  require "$runbook" 'production capture is operator-only' 'CGROUP_RETIREMENT_PRODUCTION_CAPTURE_OPERATOR_ONLY:' || return 1
  require "$runbook" 'live canary is operator-only' 'CGROUP_RETIREMENT_RETIRE_CANARY_OPERATOR_ONLY:' || return 1
  require "$runbook" 'human approval is operator-only' 'CGROUP_RETIREMENT_HUMAN_APPROVAL_OPERATOR_ONLY:' || return 1
  require "$runbook" 'landing verifier is named' 'CGROUP_RETIREMENT_LANDING_VERIFIER:' || return 1
  require "$runbook" 'contract limitation disclosure' 'It does not contact a cluster, observe' || return 1

  # Mandatory repository gates and the exact commands the operator consumes.
  require "$runbook" 'PREP range command' 'scripts/check-cgroup-retirement-gate.sh --prep PREP_BASE PREP_HEAD' || return 1
  require "$runbook" 'checked asset manifest' 'scripts/cgroup-retirement-assets.json' || return 1
  require "$runbook" 'candidate evidence command' 'scripts/verify-cgroup-retirement-evidence.sh --candidate RETIRE_HEAD' || return 1
  require "$runbook" 'candidate range command' 'scripts/check-cgroup-retirement-gate.sh --deploy --candidate RETIRE_HEAD --inputs scripts/fixtures/cgroup-retirement/gate/all-green.json' || return 1
  require "$runbook" 'aggregate rollback command' 'scripts/rehearse-cgroup-retirement-rollback.sh' || return 1
  require "$runbook" 'landing command' 'scripts/verify-cgroup-retirement-evidence.sh --landing M' || return 1
  require "$runbook" 'rearm recovery link' '[cgroup launcher re-arm recovery procedure](cgroup-launcher-rearm.md)' || return 1

  # Every outcome has a load-bearing statement; KEEP must be the pre-landing
  # default and RECOVERY may not be relabeled while restoration is incomplete.
  require "$runbook" 'KEEP default rule' 'CGROUP_RETIREMENT_KEEP_DEFAULT:' || return 1
  require "$runbook" 'RETIRE complete M rule' 'CGROUP_RETIREMENT_RETIRE_RULE:' || return 1
  require "$runbook" 'RECOVERY exclusive rule' 'CGROUP_RETIREMENT_RECOVERY_EXCLUSIVE:' || return 1
  require "$runbook" 'recovery proofs' 'CGROUP_RETIREMENT_RECOVERY_PROOFS:' || return 1
  require "$runbook" 'incomplete recovery no relabeling' 'terminal labels are refused: do not call the incomplete' || return 1
  require "$runbook" 'KEEP preservation disclosure' 'KEEP means no deletion commits land.' || return 1
  require "$runbook" 'required loss disclosure' 'separation and complete child-seccomp boundary are **lost**' || return 1
  require "$runbook" 'untested replacement disclosure' 'untested replacement; a second in-worker seccomp installer is not a' || return 1

  # Procedure order: no review, rollback, landing, retire, or keep can be
  # documented before its prerequisite stage.
  local -a steps=(
    'CGROUP_RETIREMENT_STEP: 1 PREP'
    'CGROUP_RETIREMENT_STEP: 2 EVIDENCE_CAPTURE'
    'CGROUP_RETIREMENT_STEP: 3 CANDIDATE_REVIEW'
    'CGROUP_RETIREMENT_STEP: 4 ROLLBACK_REHEARSAL'
    'CGROUP_RETIREMENT_STEP: 5 RECOVERY'
    'CGROUP_RETIREMENT_STEP: 6 LANDING'
    'CGROUP_RETIREMENT_STEP: 7 RETIRE'
    'CGROUP_RETIREMENT_STEP: 8 KEEP'
  )
  local i
  for i in "${!steps[@]}"; do
    require "$runbook" "ordered procedure step $((i + 1))" "${steps[$i]}" || return 1
    if [[ "$i" -gt 0 ]]; then before "$runbook" "procedure step $i must precede step $((i + 1))" "${steps[$((i - 1))]}" "${steps[$i]}" || return 1; fi
  done
  before "$runbook" 'KEEP law must precede RETIRE authorization' 'CGROUP_RETIREMENT_KEEP_DEFAULT:' 'CGROUP_RETIREMENT_STEP: 7 RETIRE' || return 1
  before "$runbook" 'RECOVERY rule must precede landing' 'CGROUP_RETIREMENT_RECOVERY_EXCLUSIVE:' 'CGROUP_RETIREMENT_STEP: 6 LANDING' || return 1
  before "$runbook" 'rollback rehearsal must precede landing' 'CGROUP_RETIREMENT_STEP: 4 ROLLBACK_REHEARSAL' 'CGROUP_RETIREMENT_STEP: 6 LANDING' || return 1

  # The companion is a real structured checklist rather than prose with a name.
  require "$record" 'record schema' 'CGROUP_RETIREMENT_RECORD_SCHEMA:' || return 1
  require "$record" 'record KEEP default' 'CGROUP_RETIREMENT_RECORD_KEEP_DEFAULT:' || return 1
  require "$record" 'record complete M rule' 'CGROUP_RETIREMENT_RECORD_RETIRE_COMPLETE_M:' || return 1
  require "$record" 'record operator boundary' 'CGROUP_RETIREMENT_RECORD_OPERATOR_ONLY:' || return 1
  require "$record" 'record recovery rule' 'CGROUP_RETIREMENT_RECORD_RECOVERY_EXCLUSIVE:' || return 1
  require "$record" 'record no-relabel rule' 'CGROUP_RETIREMENT_RECORD_NO_RELABEL:' || return 1
  require "$record" 'record KEEP preservation' 'CGROUP_RETIREMENT_RECORD_KEEP_NO_DELETION:' || return 1
  require "$record" 'record RETIRE landing rule' 'CGROUP_RETIREMENT_RECORD_RETIRE_ONLY_AFTER_LANDING:' || return 1

  local -a fields=(
    '`PREP_BASE:`' '`PREP_HEAD:`' '`RETIRE_BASE:`' '`RETIRE_HEAD:`' '`M:`'
    '`decision_state:`' '`prep_range_command:`' '`asset_manifest:`'
    '`candidate_evidence_command:`' '`candidate_gate_command:`' '`rollback_rehearsal_command:`'
    '`production_class_capture_operator:`' '`live_RETIRE_CANARY_rehearsal_operator:`'
    '`deployment_observation_operator:`' '`required_human_approval_operator:`'
    '`effective_required_approvals:`' '`configured_owner_coverage:`'
    '`approved_current_reviewed_head:`' '`no_bypass_or_direct_push:`'
    '`landing_verifier_command:`' '`image_oci_revision_M:`' '`render_digest_M:`'
    '`node_digest_M:`' '`workload_digest_M:`' '`pod_annotation_M:`'
    '`final_one_container_dispatch_confirmation:`' '`candidate_fault:`'
    '`post_deploy_fault:`' '`aggregate_tree_byte_identity:`' '`node_asset_restoration:`'
    '`live_launcher_leaf_restoration:`' '`rearm_runbook:`' '`terminal_state:`'
    '`loss_launcher_uid_separation:`' '`loss_child_seccomp_boundary:`'
    '`loss_second_in_worker_seccomp_installer:`'
  )
  for i in "${fields[@]}"; do require "$record" "decision record field $i" "$i" || return 1; done
  require "$record" 'record loss child seccomp value' '`lost-complete`' || return 1
  require "$record" 'record loss uid value' '`lost`' || return 1
  require "$record" 'record untested installer value' '`not-claimed`' || return 1
}

delete_line() { awk -v n="$2" 'index($0,n)==0' "$1" > "$1.tmp" && mv "$1.tmp" "$1"; }
swap_lines() {
  local f=$1 a b x y; x=$(line "$f" "$2"); y=$(line "$f" "$3")
  awk -v x="$x" -v y="$y" '{v[NR]=$0} END {t=v[x];v[x]=v[y];v[y]=t;for(i=1;i<=NR;i++)print v[i]}' "$f" > "$f.tmp" && mv "$f.tmp" "$f"
}
expect_rejected() {
  local name=$1 mutator=$2 r d out
  r="$WORK/$name.runbook"; d="$WORK/$name.record"; out="$WORK/$name.out"
  shift 2
  cp "$RUNBOOK" "$r"; cp "$RECORD" "$d"; "$mutator" "$r" "$d" "$@"
  if validate "$r" "$d" >"$out" 2>&1; then fail "negative fixture $name unexpectedly passed"; return 1; fi
  printf 'REJECTED (expected): %-38s %s\n' "$name" "$(head -n1 "$out")"
}
mutate_runbook() { delete_line "$1" "$3"; }
mutate_record() { delete_line "$2" "$3"; }
mutate_order() { swap_lines "$1" "$3" "$4"; }

validate "$RUNBOOK" "$RECORD"
# Each category gets a direct non-vacuity mutation, including a mandatory
# command, an operator boundary, a state law, loss disclosure, record field,
# and ordering rule.
expect_rejected remove-prep-gate mutate_runbook 'scripts/check-cgroup-retirement-gate.sh --prep PREP_BASE PREP_HEAD'
expect_rejected remove-landing-gate mutate_runbook 'CGROUP_RETIREMENT_LANDING_VERIFIER:'
expect_rejected remove-operator-boundary mutate_runbook 'CGROUP_RETIREMENT_OPERATOR_ONLY_BOUNDARY:'
expect_rejected remove-keep-default mutate_runbook 'CGROUP_RETIREMENT_KEEP_DEFAULT:'
expect_rejected remove-recovery-rule mutate_runbook 'CGROUP_RETIREMENT_RECOVERY_EXCLUSIVE:'
expect_rejected remove-loss-disclosure mutate_runbook 'separation and complete child-seccomp boundary are **lost**'
expect_rejected remove-record-M mutate_record '`M:`'
expect_rejected remove-record-live-leaf mutate_record '`live_launcher_leaf_restoration:`'
expect_rejected remove-record-human-approval mutate_record '`required_human_approval_operator:`'
expect_rejected reorder-landing-before-rollback mutate_order 'CGROUP_RETIREMENT_STEP: 4 ROLLBACK_REHEARSAL' 'CGROUP_RETIREMENT_STEP: 6 LANDING'

printf 'PASS: runbook_contract::cgroup_launcher_retirement_decision\n'
