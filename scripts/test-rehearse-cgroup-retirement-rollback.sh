#!/bin/sh
# Hermetic tests for aggregate cgroup-launcher retirement rollback rehearsal.
set -eu
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REHEARSE="$SCRIPT_DIR/rehearse-cgroup-retirement-rollback.sh"
PASS=0 FAIL=0
pass() { PASS=$((PASS + 1)); printf '  ok   %s\n' "$1"; }
fail() { FAIL=$((FAIL + 1)); printf '  FAIL %s\n' "$1" >&2; }
expect_ok() { label=$1 expected=$2; shift 2; out=$("$@" 2>&1) && printf '%s' "$out" | grep -F "$expected" >/dev/null && pass "$label" || fail "$label: ${out:-command failed}"; }
expect_reject() { label=$1 expected=$2; shift 2; set +e; out=$("$@" 2>&1); code=$?; set -e; [ "$code" -eq 1 ] && printf '%s' "$out" | grep -F "$expected" >/dev/null && pass "$label" || fail "$label (exit $code): $out"; }
start_branch=$(git -C "$SCRIPT_DIR/.." rev-parse --abbrev-ref HEAD)
start_status=$(git -C "$SCRIPT_DIR/.." status --porcelain)
printf 'Testing hermetic aggregate cgroup-retirement rollback rehearsal\n'
expect_ok 'reverts every candidate commit and proves aggregate identity' 'RETIRE rollback rehearsal: OK' "$REHEARSE"
expect_reject 'rejects stopping before oldest candidate' 'stopped before the oldest' "$REHEARSE" --case stop-before-oldest
expect_reject 'rejects newest-first violation' 'newest-to-oldest' "$REHEARSE" --case out-of-order
expect_reject 'rejects split forced source/test pair' 'forced source/test pair was split' "$REHEARSE" --case split-pair
expect_reject 'rejects missing node restoration' 'modeled node asset was not restored' "$REHEARSE" --case missing-node
expect_reject 'rejects mismatched tree digest' 'tracked tree digest does not match' "$REHEARSE" --case digest-mismatch
expect_reject 'rejects missing launcher leaf proof' 'launcher leaf proof is missing' "$REHEARSE" --case missing-leaf
expect_reject 'rejects malformed launcher leaf proof' 'launcher leaf proof is missing' "$REHEARSE" --case malformed-leaf
expect_reject 'rejects stale launcher leaf proof' 'launcher leaf proof is missing' "$REHEARSE" --case stale-leaf
expect_ok 'RECOVERY pauses dispatch, refuses labels, captures snapshots, then repairs' 'RECOVERY repair remains nonterminal; dispatch paused; aggregate restoration green' "$REHEARSE" --case corrupt-recovery
[ "$(git -C "$SCRIPT_DIR/.." rev-parse --abbrev-ref HEAD)" = "$start_branch" ] && [ "$(git -C "$SCRIPT_DIR/.." status --porcelain)" = "$start_status" ] && pass 'caller branch and worktree unchanged' || fail 'caller branch or worktree changed'
printf '%s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
