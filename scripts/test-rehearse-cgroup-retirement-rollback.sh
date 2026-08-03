#!/bin/sh
# Hermetic tests for aggregate cgroup-launcher retirement rollback rehearsal.
set -eu
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REHEARSE="$SCRIPT_DIR/rehearse-cgroup-retirement-rollback.sh"
PASS=0 FAIL=0
FIXTURE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/djinn/rollback-rehearsal-test-$$"
mkdir -p "$FIXTURE_DIR"
trap 'rm -rf "$FIXTURE_DIR"' EXIT HUP INT TERM
pass() { PASS=$((PASS + 1)); printf '  ok   %s\n' "$1"; }
fail() { FAIL=$((FAIL + 1)); printf '  FAIL %s\n' "$1" >&2; }
expect_ok() { label=$1 expected=$2; shift 2; out=$("$@" 2>&1) && printf '%s' "$out" | grep -F "$expected" >/dev/null && pass "$label" || fail "$label: ${out:-command failed}"; }
expect_reject() { label=$1 expected=$2; shift 2; set +e; out=$("$@" 2>&1); code=$?; set -e; [ "$code" -eq 1 ] && printf '%s' "$out" | grep -F "$expected" >/dev/null && pass "$label" || fail "$label (exit $code): $out"; }
make_fixture() {
    name=$1 mutation=$2
    node - "$SCRIPT_DIR/fixtures/cgroup-retirement/rollback/plan.json" "$FIXTURE_DIR/$name.json" "$mutation" <<'NODE'
const fs = require('fs');
const crypto = require('crypto');
const [source, target, mutation] = process.argv.slice(2);
const plan = JSON.parse(fs.readFileSync(source, 'utf8'));
const launcher = plan.assets.find((asset) => asset.role === 'launcher');
if (mutation === 'invalid-cpu') plan.launcher_leaf.cpu_max = '0 0';
if (mutation === 'unusable-quota') {
    launcher.content = "#!/bin/sh\nprintf '%s\\n' '1 100000'\n";
    plan.launcher_leaf.cpu_max = '1 100000';
    plan.launcher_leaf.source_sha256 = crypto.createHash('sha256').update(launcher.content).digest('hex');
}
if (mutation === 'wrong-output') {
    launcher.content = "#!/bin/sh\nprintf '%s\\n' '500000 100000'\n";
    plan.launcher_leaf.source_sha256 = crypto.createHash('sha256').update(launcher.content).digest('hex');
}
if (mutation === 'traversal') plan.assets.push({ path: '../../rollback-isolation-victim', phase: 'preserved', role: 'preserved', content: 'must not be written\n' });
if (mutation === 'absolute') plan.assets.push({ path: '/rollback-isolation-victim', phase: 'preserved', role: 'preserved', content: 'must not be written\n' });
if (mutation === 'empty') plan.assets.push({ path: '', phase: 'preserved', role: 'preserved', content: 'must not be written\n' });
if (mutation === 'non-normalized') plan.assets.push({ path: 'fixtures/../rollback-isolation-victim', phase: 'preserved', role: 'preserved', content: 'must not be written\n' });
fs.writeFileSync(target, `${JSON.stringify(plan, null, 2)}\n`);
NODE
}
start_branch=$(git -C "$SCRIPT_DIR/.." rev-parse --abbrev-ref HEAD)
start_status=$(git -C "$SCRIPT_DIR/.." status --porcelain)
printf 'Testing hermetic aggregate cgroup-retirement rollback rehearsal\n'
make_fixture invalid-cpu invalid-cpu
make_fixture unusable-quota unusable-quota
make_fixture wrong-output wrong-output
make_fixture traversal traversal
make_fixture absolute absolute
make_fixture empty empty
make_fixture non-normalized non-normalized
expect_ok 'reverts every candidate commit and proves aggregate identity' 'RETIRE rollback rehearsal: OK' "$REHEARSE"
expect_reject 'rejects stopping before oldest candidate' 'stopped before the oldest' "$REHEARSE" --case stop-before-oldest
expect_reject 'rejects newest-first violation' 'newest-to-oldest' "$REHEARSE" --case out-of-order
expect_reject 'rejects split forced source/test pair' 'forced source/test pair was split' "$REHEARSE" --case split-pair
expect_reject 'rejects missing node restoration' 'modeled node asset was not restored' "$REHEARSE" --case missing-node
expect_reject 'rejects mismatched tree digest' 'tracked tree digest does not match' "$REHEARSE" --case digest-mismatch
expect_reject 'rejects missing launcher leaf proof' 'launcher leaf proof is missing' "$REHEARSE" --case missing-leaf
expect_reject 'rejects malformed launcher leaf proof' 'launcher leaf proof is missing' "$REHEARSE" --case malformed-leaf
expect_reject 'rejects stale launcher leaf proof' 'launcher leaf proof is missing' "$REHEARSE" --case stale-leaf
expect_reject 'rejects unusable cpu.max fixture record' 'fixture launcher leaf is malformed or stale' "$REHEARSE" --fixture "$FIXTURE_DIR/invalid-cpu.json"
expect_reject 'rejects finite cpu.max quota below one millisecond' 'fixture launcher leaf is malformed or stale' "$REHEARSE" --fixture "$FIXTURE_DIR/unusable-quota.json"
expect_reject 'rejects launcher output that differs from expected cpu.max' 'launcher leaf proof is missing' "$REHEARSE" --fixture "$FIXTURE_DIR/wrong-output.json"
expect_reject 'rejects fixture traversal before disposable-repository writes' 'fixture asset path is unsafe' "$REHEARSE" --fixture "$FIXTURE_DIR/traversal.json"
expect_reject 'rejects absolute fixture asset paths' 'fixture asset path is unsafe' "$REHEARSE" --fixture "$FIXTURE_DIR/absolute.json"
expect_reject 'rejects empty fixture asset paths' 'fixture asset path is unsafe' "$REHEARSE" --fixture "$FIXTURE_DIR/empty.json"
expect_reject 'rejects non-normalized fixture asset paths' 'fixture asset path is unsafe' "$REHEARSE" --fixture "$FIXTURE_DIR/non-normalized.json"
expect_ok 'RECOVERY pauses dispatch, refuses labels, captures snapshots, then repairs' 'RECOVERY repair remains nonterminal; dispatch paused; aggregate restoration green' "$REHEARSE" --case corrupt-recovery
[ "$(git -C "$SCRIPT_DIR/.." rev-parse --abbrev-ref HEAD)" = "$start_branch" ] && [ "$(git -C "$SCRIPT_DIR/.." status --porcelain)" = "$start_status" ] && pass 'caller branch and worktree unchanged' || fail 'caller branch or worktree changed'
printf '%s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
