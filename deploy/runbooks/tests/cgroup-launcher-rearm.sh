#!/usr/bin/env bash
# Hermetic static contract for the cgroup-launcher re-arm runbook.
# Usage: bash deploy/runbooks/tests/cgroup-launcher-rearm.sh
#
# This test requires no cluster, no kubectl, no helm, and no credentials. It
# only reads the runbook markdown and asserts textual/ordering properties.
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
  local name=$1 document="$WORK/$1.md"
  shift
  cp "$RUNBOOK" "$document"
  local mutator=$1
  shift
  "$mutator" "$document" "$@"
  if validate "$document" >/dev/null 2>&1; then
    fail "negative fixture '$name' unexpectedly passed"
    return 1
  fi
  printf 'REJECTED (expected): %s\n' "$name"
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

swap_pass_after_step_2() { swap_matching_lines "$1" 'CGROUP_REARM_CONFORMANCE_PASS_MARKER:' 'CGROUP_REARM_STEP: 2 —'; }
expect_rejected reorder-label-before-pass swap_pass_after_step_2

# --- Regression fixture: reintroducing the banned 'label node' substring
# (e.g. an operator-facing manual mutation command) must fail validation,
# proving the anti-regression check above is not vacuous.
reintroduce_label_node_phrase() {
  local document=$1
  printf '\nkubectl label node example-node djinn.io/cgroup-writable=true\n' >>"$document"
}
expect_rejected regression-label-node-phrase reintroduce_label_node_phrase

printf 'PASS: runbook_contract::cgroup_launcher_rearm\n'
