#!/usr/bin/env bash
# Hermetic static contract for the Kueue cutover quiesce/rollback runbook.
# Usage: bash deploy/runbooks/tests/kueue-cutover-quiesce-rollback.sh
#
# No cluster, no kubectl, no helm, no credentials. It reads one markdown file
# and one shell script and asserts textual, ordering, and cross-file
# properties.
#
# WHAT A PASS HERE DOES AND DOES NOT MEAN. It asserts that the runbook CONTAINS
# and ORDERS the cutover's two laws and both sequences, and that the preflight
# it points at exists, is committed executable, and still uses the exit codes
# the runbook quotes. It observes no cluster and cannot: a pass never means a
# cutover happened, a fence held, or a task-run dispatched. Those are operator
# observations, attested in the change record.
#
# Every required element has a matching negative fixture below: deleting it,
# or reordering it, must fail validation. A contract whose assertions are not
# individually load-bearing is a contract that passes because it checks
# nothing.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUNBOOKS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DEPLOY_DIR="$(cd "$RUNBOOKS_DIR/.." && pwd)"
REPO_ROOT="$(cd "$DEPLOY_DIR/.." && pwd)"
RUNBOOK="$RUNBOOKS_DIR/kueue-cutover-quiesce-rollback.md"
PREFLIGHT="$DEPLOY_DIR/kueue/preflight.sh"
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

  # --- Existence: scope disclaimer and the two laws. ---
  require_line "$document" 'scope disclaimer (this runbook claims no rollout)' \
    'KUEUE_CUTOVER_SCOPE:' || return 1
  require_line "$document" 'RuntimeClass-before-label ordering law' \
    'KUEUE_CUTOVER_ORDERING_LAW:' || return 1
  require_line "$document" 'the wedge mechanism that makes the ordering law mandatory' \
    'KUEUE_CUTOVER_WEDGE_MECHANISM:' || return 1
  require_line "$document" 'the renderer assertion quoted verbatim' \
    'required cgroup launcher requires runtimeClassName: djinn-cgroup-writable' || return 1
  require_line "$document" 'the assertion is named as a PANIC, not a rejected Job' \
    'a renderer PANIC, not a rejected Job' || return 1
  require_line "$document" 'drain law' \
    'KUEUE_CUTOVER_DRAIN_LAW:' || return 1
  require_line "$document" 'drain law names the nonterminal build_leases population' \
    'zero nonterminal `build_leases` rows' || return 1
  require_line "$document" 'drain law names the graph-warm Job population' \
    'zero live graph-warm Jobs' || return 1
  require_line "$document" 'drain law names the Kueue Workload population' \
    'zero Kueue Workloads in the namespace' || return 1
  require_line "$document" 'symmetry rule (the same fence guards a revert)' \
    'KUEUE_CUTOVER_FENCE_IS_SYMMETRIC:' || return 1

  # --- Existence: the preflight is REQUIRED, invoked in both directions, and
  # its unreadable-population codes are declared failures rather than skips. ---
  require_line "$document" 'preflight requirement' \
    'KUEUE_CUTOVER_PREFLIGHT_REQUIRED:' || return 1
  require_line "$document" 'cutover-direction preflight invocation' \
    'deploy/kueue/preflight.sh --context <ctx> --mode cutover' || return 1
  require_line "$document" 'rollback-direction preflight invocation' \
    'deploy/kueue/preflight.sh --context <ctx> --mode rollback' || return 1
  require_line "$document" 'the exit-code roster the operator reads' \
    '10 RuntimeClass, 20 task-run Jobs, 21 graph-warm Jobs, 22 Workloads, 30 `build_leases` rows, 40 ledger unreadable, 41 API unreadable' || return 1
  require_line "$document" 'unreadable populations are failures, not skips' \
    'Codes 40 and 41 are failures, not skips' || return 1

  # --- Existence: every cutover step. ---
  require_line "$document" 'step 0 (record the rollback image/chart pair)' 'KUEUE_CUTOVER_STEP: 0 —' || return 1
  require_line "$document" 'step 1 (zero-capture prerequisite gate)' 'KUEUE_CUTOVER_STEP: 1 —' || return 1
  require_line "$document" 'step 2 (install the RuntimeClass)' 'KUEUE_CUTOVER_STEP: 2 —' || return 1
  require_line "$document" 'step 3 (pause dispatch and drain)' 'KUEUE_CUTOVER_STEP: 3 —' || return 1
  require_line "$document" 'step 4 (run the preflight)' 'KUEUE_CUTOVER_STEP: 4 —' || return 1
  require_line "$document" 'step 5 (flip authority)' 'KUEUE_CUTOVER_STEP: 5 —' || return 1
  require_line "$document" 'step 6 (resume dispatch)' 'KUEUE_CUTOVER_STEP: 6 —' || return 1
  require_line "$document" 'the one-upgrade atomicity rule' 'KUEUE_CUTOVER_ATOMIC_UPGRADE:' || return 1
  require_line "$document" 'step 0 records the pair byte-for-byte' 'byte-for-byte' || return 1
  require_line "$document" 'draining is waiting, not deleting' \
    'Draining is waiting, not deleting' || return 1

  # --- Existence: resume evidence, declared operator-collected. ---
  require_line "$document" 'resume evidence rule' 'KUEUE_RESUME_EVIDENCE:' || return 1
  require_line "$document" 'resume evidence is an actually dispatched task-run' \
    'an actually dispatched task-run reaching a running Pod' || return 1
  require_line "$document" 'resume evidence is declared operator-collected, not CI-observed' \
    'KUEUE_EVIDENCE_IS_OPERATOR_COLLECTED:' || return 1

  # --- Existence: the rollback sequence. ---
  require_line "$document" 'rollback trigger' 'KUEUE_ROLLBACK_TRIGGER:' || return 1
  require_line "$document" 'rollback step 1 (pause and drain)' 'KUEUE_ROLLBACK_STEP: 1 —' || return 1
  require_line "$document" 'rollback step 2 (preflight in the reverse direction)' 'KUEUE_ROLLBACK_STEP: 2 —' || return 1
  require_line "$document" 'rollback step 3 (restore the recorded pair)' 'KUEUE_ROLLBACK_STEP: 3 —' || return 1
  require_line "$document" 'rollback step 4 (clear stale occupying rows)' 'KUEUE_ROLLBACK_STEP: 4 —' || return 1
  require_line "$document" 'rollback step 5 (resume with the same evidence)' 'KUEUE_ROLLBACK_STEP: 5 —' || return 1
  require_line "$document" 'rollback symmetry marker' 'KUEUE_ROLLBACK_SYMMETRIC_FENCE:' || return 1
  require_line "$document" 'the RuntimeClass survives a rollback' 'KUEUE_ROLLBACK_RUNTIME_CLASS_STAYS:' || return 1
  require_line "$document" 'stale occupying rows must be cleared before resume' 'KUEUE_ROLLBACK_CLEAR_STALE_ROWS:' || return 1
  require_line "$document" 'the occupying states are enumerated, not gestured at' \
    '`granted`, `launching`, `bound`, `active` or `suspect`' || return 1

  # --- Ordering: cutover steps strictly ascending. ---
  require_before "$document" 'step 0 must precede step 1' 'KUEUE_CUTOVER_STEP: 0 —' 'KUEUE_CUTOVER_STEP: 1 —' || return 1
  require_before "$document" 'step 1 must precede step 2' 'KUEUE_CUTOVER_STEP: 1 —' 'KUEUE_CUTOVER_STEP: 2 —' || return 1
  require_before "$document" 'step 2 must precede step 3' 'KUEUE_CUTOVER_STEP: 2 —' 'KUEUE_CUTOVER_STEP: 3 —' || return 1
  require_before "$document" 'step 3 must precede step 4' 'KUEUE_CUTOVER_STEP: 3 —' 'KUEUE_CUTOVER_STEP: 4 —' || return 1
  require_before "$document" 'step 4 must precede step 5' 'KUEUE_CUTOVER_STEP: 4 —' 'KUEUE_CUTOVER_STEP: 5 —' || return 1
  require_before "$document" 'step 5 must precede step 6' 'KUEUE_CUTOVER_STEP: 5 —' 'KUEUE_CUTOVER_STEP: 6 —' || return 1

  # --- Ordering: the two laws bind the steps they govern. ---
  # The RuntimeClass install (step 2) and the ordering law that demands it must
  # both sit before the step that applies the namespace label (step 5). A
  # document that states the law after the flip has documented an incident.
  require_before "$document" 'the ordering law must precede the namespace-labelling flip (step 5)' \
    'KUEUE_CUTOVER_ORDERING_LAW:' 'KUEUE_CUTOVER_STEP: 5 —' || return 1
  require_before "$document" 'the RuntimeClass install (step 2) must precede the flip (step 5)' \
    'KUEUE_CUTOVER_STEP: 2 —' 'KUEUE_CUTOVER_STEP: 5 —' || return 1
  require_before "$document" 'the preflight requirement must precede the flip (step 5)' \
    'KUEUE_CUTOVER_PREFLIGHT_REQUIRED:' 'KUEUE_CUTOVER_STEP: 5 —' || return 1
  require_before "$document" 'the drain (step 3) must precede the preflight (step 4)' \
    'KUEUE_CUTOVER_STEP: 3 —' 'KUEUE_CUTOVER_STEP: 4 —' || return 1
  require_before "$document" 'resume evidence belongs after the resume step (step 6)' \
    'KUEUE_CUTOVER_STEP: 6 —' 'KUEUE_RESUME_EVIDENCE:' || return 1
  require_before "$document" 'the operator-collected disclaimer must follow the evidence rule it governs' \
    'KUEUE_RESUME_EVIDENCE:' 'KUEUE_EVIDENCE_IS_OPERATOR_COLLECTED:' || return 1

  # --- Ordering: rollback steps strictly ascending, fence before restore. ---
  require_before "$document" 'rollback step 1 must precede rollback step 2' \
    'KUEUE_ROLLBACK_STEP: 1 —' 'KUEUE_ROLLBACK_STEP: 2 —' || return 1
  require_before "$document" 'rollback preflight (step 2) must precede the restore (step 3)' \
    'KUEUE_ROLLBACK_STEP: 2 —' 'KUEUE_ROLLBACK_STEP: 3 —' || return 1
  require_before "$document" 'rollback restore (step 3) must precede clearing stale rows (step 4)' \
    'KUEUE_ROLLBACK_STEP: 3 —' 'KUEUE_ROLLBACK_STEP: 4 —' || return 1
  require_before "$document" 'stale rows must be cleared (step 4) before dispatch resumes (step 5)' \
    'KUEUE_ROLLBACK_STEP: 4 —' 'KUEUE_ROLLBACK_STEP: 5 —' || return 1
  require_before "$document" 'the stale-row rule must precede the resume step it gates' \
    'KUEUE_ROLLBACK_CLEAR_STALE_ROWS:' 'KUEUE_ROLLBACK_STEP: 5 —' || return 1

  # --- Anti-regression: deploy/helm/djinn/tests/cgroup-writable-render.sh
  # asserts that deploy/node/k3s/djinn-cgroup-writable-conformance.sh is the
  # SOLE file under deploy/ (outside tests/) containing 'label node' alongside
  # 'cgroup-writable'. This runbook names the RuntimeClass, so the literal
  # substring must never appear here or that ownership assertion breaks.
  if grep -Fq -- 'label node' "$document"; then
    fail 'runbook must not contain the literal substring "label node" (registers as a second label mutator in deploy/helm/djinn/tests/cgroup-writable-render.sh)'
    return 1
  fi
}

# The runbook quotes the preflight's exit codes. If the script renumbers them,
# the runbook silently starts lying to an operator reading it under pressure.
validate_exit_codes() {
  local script=$1
  local -a expected=(
    'EXIT_MISSING_RUNTIME_CLASS=10'
    'EXIT_LIVE_TASK_RUN_JOBS=20'
    'EXIT_LIVE_WARM_JOBS=21'
    'EXIT_WORKLOADS_PRESENT=22'
    'EXIT_NONTERMINAL_LEASES=30'
    'EXIT_LEDGER_UNREADABLE=40'
    'EXIT_API_UNREADABLE=41'
  )
  # Anchored, and deliberately so: an unanchored `grep -Fq` accepts the
  # assignment inside a `#` comment. Renumber to
  # `EXIT_MISSING_RUNTIME_CLASS=11` and leave
  # `# was EXIT_MISSING_RUNTIME_CLASS=10 before the renumber` and the runbook
  # now lies to an operator while this contract stays green — the 1j64 shape,
  # in the silent (presence) direction. This file's own non-vacuity mutant at
  # `expect_rejected` already uses an anchored `sed`, so it removed the real
  # line and never exercised the comment case.
  local assignment
  for assignment in "${expected[@]}"; do
    grep -Eq -- "^[[:space:]]*(readonly[[:space:]]+|declare[[:space:]]+-r[[:space:]]+|export[[:space:]]+)?${assignment}[[:space:]]*(#.*)?$" "$script" \
      || { fail "preflight no longer defines $assignment as code, but the runbook quotes that code"; return 1; }
  done
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
  # Name the assertion that fired. A fixture that fails for an unrelated
  # reason proves nothing, and that is only visible if the reason is printed.
  printf 'REJECTED (expected): %-46s %s\n' "$name" "$(head -n1 "$diagnostics")"
}

delete_matching_line() {
  local document=$1 pattern=$2 temporary="$document.tmp"
  awk -v needle="$pattern" 'index($0, needle) == 0' "$document" >"$temporary"
  mv "$temporary" "$document"
}

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

relocate_line_to_end() {
  local document=$1 pattern=$2 relocated
  relocated=$(grep -F -- "$pattern" "$document" | head -n1)
  delete_matching_line "$document" "$pattern"
  printf '\n%s\n' "$relocated" >>"$document"
}

# --- The committed artefacts. -----------------------------------------------
[ -f "$RUNBOOK" ] || { printf 'FAIL: runbook is missing: %s\n' "$RUNBOOK" >&2; exit 1; }
[ -f "$PREFLIGHT" ] || { printf 'FAIL: the runbook points at a preflight that does not exist: %s\n' "$PREFLIGHT" >&2; exit 1; }

# The COMMITTED mode, from the tree object. `git ls-files -s` reports an index
# mode that need not survive into a commit.
preflight_entry=$(git -C "$REPO_ROOT" ls-tree HEAD deploy/kueue/preflight.sh)
case "$preflight_entry" in
  100755\ *) ;;
  *) printf 'FAIL: deploy/kueue/preflight.sh is not committed executable; git ls-tree HEAD reports: %s\n' "$preflight_entry" >&2; exit 1 ;;
esac
contract_entry=$(git -C "$REPO_ROOT" ls-tree HEAD deploy/runbooks/tests/kueue-cutover-quiesce-rollback.sh)
case "$contract_entry" in
  100755\ *) ;;
  *) printf 'FAIL: this contract is not committed executable; git ls-tree HEAD reports: %s\n' "$contract_entry" >&2; exit 1 ;;
esac

validate "$RUNBOOK"
validate_exit_codes "$PREFLIGHT"

# --- Cross-file non-vacuity: the exit-code drift check must actually fire. ---
RENUMBERED="$WORK/preflight-renumbered.sh"
sed 's/^EXIT_MISSING_RUNTIME_CLASS=10$/EXIT_MISSING_RUNTIME_CLASS=11/' "$PREFLIGHT" >"$RENUMBERED"
grep -Fq -- 'EXIT_MISSING_RUNTIME_CLASS=11' "$RENUMBERED" || { printf 'FAIL: renumbering mutant was not constructed\n' >&2; exit 1; }
if validate_exit_codes "$RENUMBERED" >"$WORK/renumbered.err" 2>&1; then
  printf 'FAIL: the exit-code drift check accepted a renumbered preflight\n' >&2
  exit 1
fi
printf 'REJECTED (expected): %-46s %s\n' 'preflight/renumbered-exit-code' "$(head -n1 "$WORK/renumbered.err")"

# --- Existence fixtures: deleting any required element must fail. ------------
expect_rejected scope-disclaimer delete_matching_line 'KUEUE_CUTOVER_SCOPE:'
expect_rejected ordering-law delete_matching_line 'KUEUE_CUTOVER_ORDERING_LAW:'
expect_rejected wedge-mechanism delete_matching_line 'KUEUE_CUTOVER_WEDGE_MECHANISM:'
expect_rejected drain-law delete_matching_line 'KUEUE_CUTOVER_DRAIN_LAW:'
expect_rejected fence-symmetry delete_matching_line 'KUEUE_CUTOVER_FENCE_IS_SYMMETRIC:'
expect_rejected preflight-required delete_matching_line 'KUEUE_CUTOVER_PREFLIGHT_REQUIRED:'
expect_rejected cutover-step-0 delete_matching_line 'KUEUE_CUTOVER_STEP: 0 —'
expect_rejected cutover-step-1 delete_matching_line 'KUEUE_CUTOVER_STEP: 1 —'
expect_rejected cutover-step-2 delete_matching_line 'KUEUE_CUTOVER_STEP: 2 —'
expect_rejected cutover-step-3 delete_matching_line 'KUEUE_CUTOVER_STEP: 3 —'
expect_rejected cutover-step-4 delete_matching_line 'KUEUE_CUTOVER_STEP: 4 —'
expect_rejected cutover-step-5 delete_matching_line 'KUEUE_CUTOVER_STEP: 5 —'
expect_rejected cutover-step-6 delete_matching_line 'KUEUE_CUTOVER_STEP: 6 —'
expect_rejected atomic-upgrade delete_matching_line 'KUEUE_CUTOVER_ATOMIC_UPGRADE:'
expect_rejected resume-evidence delete_matching_line 'KUEUE_RESUME_EVIDENCE:'
expect_rejected operator-collected-disclaimer delete_matching_line 'KUEUE_EVIDENCE_IS_OPERATOR_COLLECTED:'
expect_rejected rollback-trigger delete_matching_line 'KUEUE_ROLLBACK_TRIGGER:'
expect_rejected rollback-step-1 delete_matching_line 'KUEUE_ROLLBACK_STEP: 1 —'
expect_rejected rollback-step-2 delete_matching_line 'KUEUE_ROLLBACK_STEP: 2 —'
expect_rejected rollback-step-3 delete_matching_line 'KUEUE_ROLLBACK_STEP: 3 —'
expect_rejected rollback-step-4 delete_matching_line 'KUEUE_ROLLBACK_STEP: 4 —'
expect_rejected rollback-step-5 delete_matching_line 'KUEUE_ROLLBACK_STEP: 5 —'
expect_rejected rollback-symmetry delete_matching_line 'KUEUE_ROLLBACK_SYMMETRIC_FENCE:'
expect_rejected rollback-runtimeclass-stays delete_matching_line 'KUEUE_ROLLBACK_RUNTIME_CLASS_STAYS:'
expect_rejected rollback-clear-stale-rows delete_matching_line 'KUEUE_ROLLBACK_CLEAR_STALE_ROWS:'
expect_rejected renderer-assertion-quote delete_matching_line \
  'required cgroup launcher requires runtimeClassName: djinn-cgroup-writable'
expect_rejected exit-code-roster delete_matching_line \
  '10 RuntimeClass, 20 task-run Jobs, 21 graph-warm Jobs, 22 Workloads, 30 `build_leases` rows, 40 ledger unreadable, 41 API unreadable'
expect_rejected unreadable-is-not-a-skip delete_matching_line 'Codes 40 and 41 are failures, not skips'
expect_rejected cutover-invocation delete_matching_line \
  'deploy/kueue/preflight.sh --context <ctx> --mode cutover'
expect_rejected rollback-invocation delete_matching_line \
  'deploy/kueue/preflight.sh --context <ctx> --mode rollback'
expect_rejected occupying-states-enumerated delete_matching_line \
  '`granted`, `launching`, `bound`, `active` or `suspect`'
expect_rejected drain-is-waiting delete_matching_line 'Draining is waiting, not deleting'

# --- Ordering fixtures: nothing is deleted, only the sequence changes. -------
swap_flip_before_preflight() { swap_matching_lines "$1" 'KUEUE_CUTOVER_STEP: 4 —' 'KUEUE_CUTOVER_STEP: 5 —'; }
expect_rejected reorder-flip-before-preflight swap_flip_before_preflight

swap_drain_after_preflight() { swap_matching_lines "$1" 'KUEUE_CUTOVER_STEP: 3 —' 'KUEUE_CUTOVER_STEP: 4 —'; }
expect_rejected reorder-drain-after-preflight swap_drain_after_preflight

swap_runtimeclass_after_flip() { swap_matching_lines "$1" 'KUEUE_CUTOVER_STEP: 2 —' 'KUEUE_CUTOVER_STEP: 5 —'; }
expect_rejected reorder-runtimeclass-after-flip swap_runtimeclass_after_flip

# The ordering law itself, moved after the step it governs. Every step marker
# stays put, so the law's own ordering assertion is the only one that can fire.
relocate_ordering_law() { relocate_line_to_end "$1" 'KUEUE_CUTOVER_ORDERING_LAW:'; }
expect_rejected relocate-ordering-law-after-flip relocate_ordering_law

relocate_preflight_requirement() { relocate_line_to_end "$1" 'KUEUE_CUTOVER_PREFLIGHT_REQUIRED:'; }
expect_rejected relocate-preflight-requirement-after-flip relocate_preflight_requirement

swap_rollback_restore_before_preflight() { swap_matching_lines "$1" 'KUEUE_ROLLBACK_STEP: 2 —' 'KUEUE_ROLLBACK_STEP: 3 —'; }
expect_rejected reorder-rollback-restore-before-preflight swap_rollback_restore_before_preflight

swap_rollback_resume_before_clear() { swap_matching_lines "$1" 'KUEUE_ROLLBACK_STEP: 4 —' 'KUEUE_ROLLBACK_STEP: 5 —'; }
expect_rejected reorder-rollback-resume-before-clear swap_rollback_resume_before_clear

relocate_clear_stale_rows() { relocate_line_to_end "$1" 'KUEUE_ROLLBACK_CLEAR_STALE_ROWS:'; }
expect_rejected relocate-clear-stale-rows-after-resume relocate_clear_stale_rows

# --- Regression fixture: the banned substring must be caught. ---------------
reintroduce_label_node_phrase() {
  local document=$1
  printf '\nkubectl label node example-node djinn.io/cgroup-writable=true\n' >>"$document"
}
expect_rejected regression-label-node-phrase reintroduce_label_node_phrase

printf 'PASS: runbook_contract::kueue_cutover_quiesce_rollback\n'
