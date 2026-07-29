#!/usr/bin/env bash
# Hermetic static contract for the cgroup-launcher re-arm runbook.
# Usage: bash deploy/runbooks/tests/cgroup-launcher-rearm.sh
#
# This test requires no cluster, no kubectl, no helm, and no credentials. It
# only reads the runbook markdown and asserts textual/ordering properties.
#
# WHAT A PASS HERE DOES AND DOES NOT MEAN. This check asserts that the runbook
# CONTAINS and ORDERS the re-arm's verification rules. It does not observe
# production and cannot: it reads one markdown file. A pass means the rollout
# evidence contract is written down in the required order; it never means a
# Pod was Running, a cgroup was read, 100 periods elapsed, or the board
# dispatched for an hour. Those are post-merge operator observations, attested
# in the change record, and no repository check can stand in for them.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUNBOOK="$SCRIPT_DIR/../cgroup-launcher-rearm.md"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  return 1
}

require_line() {
  local document=$1 description=$2 pattern=$3
  grep -Fq -- "$pattern" "$document" || { fail "missing $description"; return 1; }
}

first_line() {
  local document=$1 pattern=$2
  grep -nF -- "$pattern" "$document" | head -n1 | cut -d: -f1
}

require_before() {
  local document=$1 description=$2 earlier_pattern=$3 later_pattern=$4
  local earlier_line later_line
  earlier_line=$(first_line "$document" "$earlier_pattern")
  later_line=$(first_line "$document" "$later_pattern")
  if [[ -z "$earlier_line" || -z "$later_line" ]]; then
    fail "$description: pattern not found"
    return 1
  fi
  [[ "$earlier_line" -lt "$later_line" ]] || { fail "$description"; return 1; }
}

validate() {
  local document=$1

  # --- Existence: the outage declaration and the required drain. ---
  require_line "$document" 'outage declaration' \
    'CGROUP_REARM_OUTAGE_DECLARATION: systemctl restart k3s bounces the entire single-node production cluster.' || return 1
  require_line "$document" 'required dispatch pause-and-drain marker' \
    'CGROUP_REARM_DRAIN_REQUIRED:' || return 1

  # --- Existence: preparation step plus all six numbered steps. ---
  require_line "$document" 'preparation step (step 0: install RuntimeClass, launcher disarmed)' \
    'CGROUP_REARM_STEP: 0' || return 1
  require_line "$document" 'step 1 (conformance install + systemctl restart k3s)' \
    'CGROUP_REARM_STEP: 1 —' || return 1
  require_line "$document" 'step 2 (djinn.io/cgroup-writable=true applied by the conformance script)' \
    'CGROUP_REARM_STEP: 2 —' || return 1
  require_line "$document" 'step 2 states the conformance script is the sole owner of the eligibility marker' \
    'sole owner' || return 1
  require_line "$document" 'step 2 states an operator must never set or clear the marker by hand' \
    'An operator must never set or clear this marker by hand' || return 1
  require_line "$document" 'step 3 (cgroupWritable.runtimeClass.enabled: true)' \
    'CGROUP_REARM_STEP: 3 —' || return 1
  require_line "$document" 'step 4 (cgroupWritable.taskRuns.enabled: true)' \
    'CGROUP_REARM_STEP: 4 —' || return 1
  require_line "$document" 'step 5 (cgroupLauncher.mode: required)' \
    'CGROUP_REARM_STEP: 5 —' || return 1
  require_line "$document" 'step 6 (verification)' \
    'CGROUP_REARM_STEP: 6 —' || return 1

  require_line "$document" 'literal conformance PASS marker' \
    'CGROUP_REARM_CONFORMANCE_PASS_MARKER: CONFORMANCE: PASS' || return 1

  # --- Existence: rollback/recovery branch with an explicit byte-for-byte restore. ---
  require_line "$document" 'rollback/recovery branch marker' \
    'CGROUP_REARM_ROLLBACK:' || return 1
  require_line "$document" 'rollback restore is specified byte-for-byte, not just an unlabel' \
    'byte-for-byte' || return 1
  require_line "$document" 'rollback restore is verified with cmp before proceeding' \
    'cmp /var/lib/rancher/k3s/agent/etc/containerd/config.toml.tmpl.pre-rearm-backup' || return 1

  # --- Existence: verification requires kernel evidence, not a status field. ---
  require_line "$document" 'verification requires kernel evidence, not a status field' \
    'CGROUP_REARM_VERIFICATION_REQUIRES_KERNEL_EVIDENCE:' || return 1
  require_line "$document" 'cat cpu.max alone is explicitly rejected as evidence' \
    'cat cpu.max` alone never satisfies this step' || return 1
  require_line "$document" 'birth cpu.max value 25000 100000' \
    '25000 100000' || return 1
  require_line "$document" 'post-lift cpu.max value 400000 100000' \
    '400000 100000' || return 1
  require_line "$document" 'nr_throttled/throttled_usec invariance over >=100 further nr_periods' \
    'nr_throttled and throttled_usec must be identical' || return 1

  # --- Existence: the five rollout-evidence rules, as an ordered contract. ---
  # These assert the DOCUMENT states them. Nothing here observes production;
  # see the header. The markers exist so ordering is checkable without
  # depending on prose that may legitimately be reworded.
  require_line "$document" 'verification evidence is declared operator-collected post-merge, not CI-observed' \
    'CGROUP_REARM_VERIFICATION_EVIDENCE_IS_OPERATOR_COLLECTED:' || return 1
  require_line "$document" 'rule 1: exact cpu.max expectations (25000 100000 at birth, 400000 100000 after a fenced lift)' \
    'CGROUP_REARM_VERIFY_ORDER_1_CPU_MAX:' || return 1
  require_line "$document" 'rule 2: wrong-fence invariant (wrong fence token rejected, cpu.max left clamped)' \
    'CGROUP_REARM_VERIFY_ORDER_2_WRONG_FENCE:' || return 1
  require_line "$document" 'rule 3: 100-period cpu.stat sampling rule over a wall-clock window' \
    'CGROUP_REARM_VERIFY_ORDER_3_HUNDRED_PERIODS:' || return 1
  require_line "$document" 'rule 4: one-hour observation rule (board dispatches remain > 0 for the hour after the arm)' \
    'CGROUP_REARM_VERIFY_ORDER_4_ONE_HOUR_DISPATCH:' || return 1
  require_line "$document" 'rule 5: abort/rollback to cgroupLauncher.mode: disabled on any failure above' \
    'CGROUP_REARM_VERIFY_ORDER_5_ABORT_ROLLBACK:' || return 1
  # The preparation step also runs `--set cgroupLauncher.mode=disabled`, so that
  # flag alone proves nothing about rule 5. Pin the abort instruction itself.
  require_line "$document" 'rule 5 instructs an abort-and-roll-back on failure' \
    'abort the re-arm and roll the launcher back' || return 1
  require_line "$document" 'rule 2 states why the wrong-fence check is the non-vacuity check' \
    'looks identical to a working clamp' || return 1
  require_line "$document" 'rule 5 forbids leaving production armed on unproven delegation' \
    'never be left armed on unproven delegation' || return 1

  # --- Ordering: preparation, then steps 0..6 strictly ascending. ---
  require_before "$document" 'step 0 (RuntimeClass install) must precede step 1 (conformance)' \
    'CGROUP_REARM_STEP: 0' 'CGROUP_REARM_STEP: 1 —' || return 1
  require_before "$document" 'step 1 must precede step 2' \
    'CGROUP_REARM_STEP: 1 —' 'CGROUP_REARM_STEP: 2 —' || return 1
  require_before "$document" 'step 2 must precede step 3' \
    'CGROUP_REARM_STEP: 2 —' 'CGROUP_REARM_STEP: 3 —' || return 1
  require_before "$document" 'step 3 must precede step 4' \
    'CGROUP_REARM_STEP: 3 —' 'CGROUP_REARM_STEP: 4 —' || return 1
  require_before "$document" 'step 4 must precede step 5' \
    'CGROUP_REARM_STEP: 4 —' 'CGROUP_REARM_STEP: 5 —' || return 1
  require_before "$document" 'step 5 must precede step 6' \
    'CGROUP_REARM_STEP: 5 —' 'CGROUP_REARM_STEP: 6 —' || return 1

  # --- Ordering: the five evidence rules, inside the verification step. ---
  # The rules are a sequence, not a set. cpu.max expectations come first
  # because everything after them is stated relative to those values; the
  # wrong-fence invariant must follow them (it is what makes the birth value
  # non-vacuous); the 100-period window measures the lift the previous rules
  # established; the one-hour dispatch observation only exists after the arm;
  # and abort/rollback closes the sequence by covering the failure of any of
  # them. The whole block must sit after the arm step, never before it.
  # 'step 5 must precede step 6' above already pins the verification section
  # after the arm step; it is not repeated here.
  require_before "$document" 'the evidence rules must sit inside the verification step (step 6)' \
    'CGROUP_REARM_STEP: 6 —' 'CGROUP_REARM_VERIFY_ORDER_1_CPU_MAX:' || return 1
  require_before "$document" 'rule 1 (cpu.max expectations) must precede rule 2 (wrong-fence invariant)' \
    'CGROUP_REARM_VERIFY_ORDER_1_CPU_MAX:' 'CGROUP_REARM_VERIFY_ORDER_2_WRONG_FENCE:' || return 1
  require_before "$document" 'rule 2 (wrong-fence invariant) must precede rule 3 (100-period sampling)' \
    'CGROUP_REARM_VERIFY_ORDER_2_WRONG_FENCE:' 'CGROUP_REARM_VERIFY_ORDER_3_HUNDRED_PERIODS:' || return 1
  require_before "$document" 'rule 3 (100-period sampling) must precede rule 4 (one-hour dispatch observation)' \
    'CGROUP_REARM_VERIFY_ORDER_3_HUNDRED_PERIODS:' 'CGROUP_REARM_VERIFY_ORDER_4_ONE_HOUR_DISPATCH:' || return 1
  require_before "$document" 'rule 4 (one-hour dispatch observation) must precede rule 5 (abort/rollback)' \
    'CGROUP_REARM_VERIFY_ORDER_4_ONE_HOUR_DISPATCH:' 'CGROUP_REARM_VERIFY_ORDER_5_ABORT_ROLLBACK:' || return 1
  require_before "$document" 'the operator-collected disclaimer must precede the evidence rules it governs' \
    'CGROUP_REARM_VERIFICATION_EVIDENCE_IS_OPERATOR_COLLECTED:' 'CGROUP_REARM_VERIFY_ORDER_1_CPU_MAX:' || return 1

  # --- Ordering: the RuntimeClass must exist before conformance can reference it. ---
  # The preparation step is where runtimeClass.enabled=true is first set; it must
  # be textually before step 1, which is already checked above via STEP: 0 < STEP: 1.
  # Reinforce with the literal values-flag ordering too.
  require_before "$document" 'cgroupWritable.runtimeClass.enabled=true (preparation) must precede conformance install' \
    'cgroupWritable.runtimeClass.enabled=true' 'kubectl apply -f deploy/node/cgroup-writable-conformance-probe.yaml' || return 1

  # --- Ordering: dispatch pause-and-drain must precede the cluster-bouncing restart. ---
  require_before "$document" 'drain requirement must precede step 1 (the systemctl restart k3s step)' \
    'CGROUP_REARM_DRAIN_REQUIRED:' 'CGROUP_REARM_STEP: 1 —' || return 1
  require_before "$document" 'outage declaration must precede step 1 (the systemctl restart k3s step)' \
    'CGROUP_REARM_OUTAGE_DECLARATION:' 'CGROUP_REARM_STEP: 1 —' || return 1

  # --- Ordering: conformance PASS must gate step 2 (the node label). ---
  require_before "$document" 'conformance PASS marker must appear within/after step 1' \
    'CGROUP_REARM_STEP: 1 —' 'CGROUP_REARM_CONFORMANCE_PASS_MARKER:' || return 1
  require_before "$document" 'node label (step 2) must come only after the conformance PASS marker' \
    'CGROUP_REARM_CONFORMANCE_PASS_MARKER:' 'CGROUP_REARM_STEP: 2 —' || return 1

  # --- Anti-regression: deploy/helm/djinn/tests/cgroup-writable-render.sh
  # enforces that deploy/node/k3s/djinn-cgroup-writable-conformance.sh is the
  # SOLE file under deploy/ that mutates djinn.io/cgroup-writable, via a crude
  # substring scan for 'label node' co-occurring with 'cgroup-writable' or
  # '$LABEL'. This runbook must never instruct an operator to run that
  # mutation by hand, so the literal substring must never reappear here.
  if grep -Fq -- 'label node' "$document"; then
    fail 'runbook must not contain the literal substring "label node" (registers as a second label mutator in deploy/helm/djinn/tests/cgroup-writable-render.sh)'
    return 1
  fi
}

expect_rejected() {
  local name=$1 document="$WORK/$1.md" diagnostics="$WORK/$1.err"
  shift
  cp "$RUNBOOK" "$document"
  local mutator=$1
  shift
  "$mutator" "$document" "$@"
  if validate "$document" >"$diagnostics" 2>&1; then
    fail "negative fixture '$name' unexpectedly passed"
    return 1
  fi
  # Print the rejection reason, not just the fixture name: a fixture that
  # fails for an unrelated reason is a fixture that proves nothing, and that
  # is only visible if the assertion that fired is named in the output.
  printf 'REJECTED (expected): %-44s %s\n' "$name" "$(head -n1 "$diagnostics")"
}

delete_matching_line() {
  local document=$1 pattern=$2 temporary="$document.tmp"
  awk -v needle="$pattern" 'index($0, needle) == 0' "$document" >"$temporary"
  mv "$temporary" "$document"
}

# Swaps the content of the two (first-matching) lines containing pattern_a and
# pattern_b, without touching any other line. Used to build ordering-violation
# fixtures that delete nothing but still break the required sequence.
swap_matching_lines() {
  local document=$1 pattern_a=$2 pattern_b=$3 temporary="$document.tmp"
  local line_a line_b
  line_a=$(first_line "$document" "$pattern_a")
  line_b=$(first_line "$document" "$pattern_b")
  if [[ -z "$line_a" || -z "$line_b" ]]; then
    fail "swap_matching_lines: pattern not found in $document"
    return 1
  fi
  awk -v a="$line_a" -v b="$line_b" '
    { lines[NR] = $0 }
    END {
      tmp = lines[a]; lines[a] = lines[b]; lines[b] = tmp
      for (i = 1; i <= NR; i++) print lines[i]
    }
  ' "$document" >"$temporary"
  mv "$temporary" "$document"
}

# Relocates the whole block [section_start, section_end) so that it begins at
# the line currently holding destination_pattern, which must appear BEFORE the
# block. Deletes nothing: it only moves a section, which is exactly the way a
# reordering regression arrives in a document.
move_section_before() {
  local document=$1 section_start=$2 section_end=$3 destination=$4 temporary="$document.tmp"
  local start_line end_line destination_line
  start_line=$(first_line "$document" "$section_start")
  end_line=$(first_line "$document" "$section_end")
  destination_line=$(first_line "$document" "$destination")
  if [[ -z "$start_line" || -z "$end_line" || -z "$destination_line" ]]; then
    fail "move_section_before: pattern not found in $document"
    return 1
  fi
  if [[ "$destination_line" -ge "$start_line" || "$end_line" -le "$start_line" ]]; then
    fail "move_section_before: destination must precede the block in $document"
    return 1
  fi
  awk -v s="$start_line" -v e="$end_line" -v d="$destination_line" '
    { lines[NR] = $0 }
    END {
      for (i = 1; i < d; i++) print lines[i]
      for (i = s; i < e; i++) print lines[i]
      for (i = d; i < s; i++) print lines[i]
      for (i = e; i <= NR; i++) print lines[i]
    }
  ' "$document" >"$temporary"
  mv "$temporary" "$document"
}

# The repository document must satisfy the complete static contract without a
# cluster, kubectl, helm binary, credentials, or a real node.
validate "$RUNBOOK"

# --- Existence fixtures: deleting any one required element must fail validation. ---
expect_rejected outage-declaration delete_matching_line \
  'CGROUP_REARM_OUTAGE_DECLARATION: systemctl restart k3s bounces the entire single-node production cluster.'
expect_rejected drain-required delete_matching_line 'CGROUP_REARM_DRAIN_REQUIRED:'
expect_rejected step-0 delete_matching_line 'CGROUP_REARM_STEP: 0'
expect_rejected step-1 delete_matching_line 'CGROUP_REARM_STEP: 1 —'
expect_rejected step-2 delete_matching_line 'CGROUP_REARM_STEP: 2 —'
expect_rejected step-3 delete_matching_line 'CGROUP_REARM_STEP: 3 —'
expect_rejected step-4 delete_matching_line 'CGROUP_REARM_STEP: 4 —'
expect_rejected step-5 delete_matching_line 'CGROUP_REARM_STEP: 5 —'
expect_rejected step-6 delete_matching_line 'CGROUP_REARM_STEP: 6 —'
expect_rejected conformance-pass-marker delete_matching_line \
  'CGROUP_REARM_CONFORMANCE_PASS_MARKER: CONFORMANCE: PASS'
expect_rejected step-2-sole-owner delete_matching_line 'sole owner'
expect_rejected step-2-never-by-hand delete_matching_line \
  'An operator must never set or clear this marker by hand'
expect_rejected rollback-marker delete_matching_line 'CGROUP_REARM_ROLLBACK:'
expect_rejected rollback-byte-for-byte delete_matching_line 'byte-for-byte'
expect_rejected rollback-cmp-verification delete_matching_line \
  'cmp /var/lib/rancher/k3s/agent/etc/containerd/config.toml.tmpl.pre-rearm-backup'
expect_rejected verification-kernel-evidence-marker delete_matching_line \
  'CGROUP_REARM_VERIFICATION_REQUIRES_KERNEL_EVIDENCE:'
expect_rejected verification-cpu-max-alone-rejected delete_matching_line \
  'cat cpu.max` alone never satisfies this step'
expect_rejected verification-birth-cpu-max delete_matching_line '25000 100000'
expect_rejected verification-lift-cpu-max delete_matching_line '400000 100000'
expect_rejected verification-throttle-invariance delete_matching_line \
  'nr_throttled and throttled_usec must be identical'

# --- Existence fixtures for the five rollout-evidence rules. Deleting any one
# rule must fail validation and NAME that rule, so the rules are individually
# load-bearing rather than collectively satisfied by the section existing.
expect_rejected verification-operator-collected-disclaimer delete_matching_line \
  'CGROUP_REARM_VERIFICATION_EVIDENCE_IS_OPERATOR_COLLECTED:'
expect_rejected verification-rule-1-cpu-max delete_matching_line \
  'CGROUP_REARM_VERIFY_ORDER_1_CPU_MAX:'
expect_rejected verification-rule-2-wrong-fence delete_matching_line \
  'CGROUP_REARM_VERIFY_ORDER_2_WRONG_FENCE:'
expect_rejected verification-rule-2-non-vacuity-rationale delete_matching_line \
  'looks identical to a working clamp'
expect_rejected verification-rule-3-hundred-periods delete_matching_line \
  'CGROUP_REARM_VERIFY_ORDER_3_HUNDRED_PERIODS:'
expect_rejected verification-rule-4-one-hour-dispatch delete_matching_line \
  'CGROUP_REARM_VERIFY_ORDER_4_ONE_HOUR_DISPATCH:'
expect_rejected verification-rule-5-abort-rollback delete_matching_line \
  'CGROUP_REARM_VERIFY_ORDER_5_ABORT_ROLLBACK:'
expect_rejected verification-rule-5-abort-instruction delete_matching_line \
  'abort the re-arm and roll the launcher back'
expect_rejected verification-rule-5-unproven-delegation delete_matching_line \
  'never be left armed on unproven delegation'

# --- Ordering fixtures: swapping step markers must fail validation even
# though no content is deleted. This is the specific adversary objection:
# the RuntimeClass must exist (step 0) before conformance can reference it
# (step 1), and the drain must precede the cluster-bouncing restart.
swap_step_0_and_1() { swap_matching_lines "$1" 'CGROUP_REARM_STEP: 0' 'CGROUP_REARM_STEP: 1 —'; }
expect_rejected reorder-step-0-after-step-1 swap_step_0_and_1

swap_step_5_and_6() { swap_matching_lines "$1" 'CGROUP_REARM_STEP: 5 —' 'CGROUP_REARM_STEP: 6 —'; }
expect_rejected reorder-step-5-after-step-6 swap_step_5_and_6

swap_drain_after_step_1() { swap_matching_lines "$1" 'CGROUP_REARM_DRAIN_REQUIRED:' 'CGROUP_REARM_STEP: 1 —'; }
expect_rejected reorder-drain-after-restart swap_drain_after_step_1

# The swap above also drags step 1's marker above step 0's, so it is rejected
# by the step-ordering check before the drain check is ever reached. Relocating
# only the drain marker leaves every step marker in place, so the drain rule is
# the sole assertion that can fire.
relocate_drain_marker_to_end() {
  local document=$1 marker='CGROUP_REARM_DRAIN_REQUIRED:'
  local relocated
  relocated=$(grep -F -- "$marker" "$document" | head -n1)
  delete_matching_line "$document" "$marker"
  printf '\n%s\n' "$relocated" >>"$document"
}
expect_rejected relocate-drain-after-restart relocate_drain_marker_to_end

swap_pass_after_step_2() { swap_matching_lines "$1" 'CGROUP_REARM_CONFORMANCE_PASS_MARKER:' 'CGROUP_REARM_STEP: 2 —'; }
expect_rejected reorder-label-before-pass swap_pass_after_step_2

# --- Ordering fixtures for the evidence rules. Nothing is deleted; only the
# sequence changes. Moving the entire verification section above the arm step
# is the realistic regression: every rule is still present and a set-based
# check would still pass, but the document now tells an operator to verify a
# launcher that has not been armed yet.
move_verification_before_arm() {
  move_section_before "$1" \
    '### Step 6: verification' \
    '## Rollback / recovery branch' \
    '### Step 5: arm the launcher'
}
expect_rejected reorder-verification-before-arm move_verification_before_arm

swap_rule_1_and_2() { swap_matching_lines "$1" 'CGROUP_REARM_VERIFY_ORDER_1_CPU_MAX:' 'CGROUP_REARM_VERIFY_ORDER_2_WRONG_FENCE:'; }
expect_rejected reorder-wrong-fence-before-cpu-max swap_rule_1_and_2

swap_rule_3_and_4() { swap_matching_lines "$1" 'CGROUP_REARM_VERIFY_ORDER_3_HUNDRED_PERIODS:' 'CGROUP_REARM_VERIFY_ORDER_4_ONE_HOUR_DISPATCH:'; }
expect_rejected reorder-one-hour-before-hundred-periods swap_rule_3_and_4

swap_rule_4_and_5() { swap_matching_lines "$1" 'CGROUP_REARM_VERIFY_ORDER_4_ONE_HOUR_DISPATCH:' 'CGROUP_REARM_VERIFY_ORDER_5_ABORT_ROLLBACK:'; }
expect_rejected reorder-abort-before-one-hour swap_rule_4_and_5

# --- Regression fixture: reintroducing the banned 'label node' substring
# (e.g. an operator-facing manual mutation command) must fail validation,
# proving the anti-regression check above is not vacuous.
reintroduce_label_node_phrase() {
  local document=$1
  printf '\nkubectl label node example-node djinn.io/cgroup-writable=true\n' >>"$document"
}
expect_rejected regression-label-node-phrase reintroduce_label_node_phrase

printf 'PASS: runbook_contract::cgroup_launcher_rearm\n'
