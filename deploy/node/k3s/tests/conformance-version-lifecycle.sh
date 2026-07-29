#!/usr/bin/env bash
# Hermetic contract for the conformance script's version preflight, its
# restore-on-failure path, and the live runtime-table validator. Every case
# drives the real script through recording stubs; the restart command is the
# observable that proves the preflight aborts before k3s is ever touched.
# Usage: bash deploy/node/k3s/tests/conformance-version-lifecycle.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NODE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_DIR="$(cd "$NODE_DIR/../../.." && pwd)"
FIXTURES="$SCRIPT_DIR/fixtures/containerd"
CONFORMANCE="$NODE_DIR/djinn-cgroup-writable-conformance.sh"
NODE='fixture-node'
PASS_LINE='PASS node=fixture-node handler=runc-cgroupwritable cgroup_root=/ writable=true isolated=true worker_denials=true'

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

failures=0
fail() { printf 'FAIL: %s\n' "$*" >&2; failures=$((failures + 1)); }
ok() { printf 'ok %s\n' "$*"; }

expect_eq() {
  local what=$1 expected=$2 actual=$3
  if [ "$expected" = "$actual" ]; then ok "$what"; else
    fail "$what: expected [$expected], got [$actual]"
  fi
}

make_stubs() {
  local dir=$1 log=$2 post_live=$3 live=$4 exec_status=$5
  mkdir -p "$dir"
  cat >"$dir/id" <<'EOF'
#!/usr/bin/env bash
[ "$1" = -u ] && { echo 0; exit 0; }
exec /usr/bin/id "$@"
EOF
  # k3s regenerates the live configuration from the installed template. The
  # fixture stands in for that render so the validator sees a real file.
  cat >"$dir/systemctl" <<EOF
#!/usr/bin/env bash
set -euo pipefail
printf 'restart %s\n' "\$*" >>'$log'
cp '$post_live' '$live'
EOF
  cat >"$dir/kubectl" <<EOF
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "\$*" >>'$log'
case "\$1" in
  label) exit 0 ;;
  get)
    if [ "\$2" = node ]; then exit 0; fi
    if [ "\$2" = pod ] && [ "\${*: -2}" = '-o json' ]; then exit 0; fi
    if [ "\$2" = pod ]; then printf '$NODE|$NODE'; exit 0; fi
    ;;
  apply) cat >/dev/null; exit 0 ;;
  wait) exit 0 ;;
  exec) exit $exec_status ;;
  delete) exit 0 ;;
esac
exit 0
EOF
  chmod +x "$dir/id" "$dir/systemctl" "$dir/kubectl"
}

# Every case is described by the live configuration the node starts with, the
# one k3s renders after a restart, and the template wiring under test.
CASE_DIR='' STATUS=0 LOG='' LIVE='' DEST=''
run_case() {
  local name=$1 pre_live=$2 post_live=$3 exec_status=$4
  shift 4
  CASE_DIR="$WORK/$name"
  LOG="$CASE_DIR/log"
  LIVE="$CASE_DIR/live.toml"
  mkdir -p "$CASE_DIR/install"
  : >"$LOG"
  cp "$FIXTURES/$pre_live" "$LIVE"
  make_stubs "$CASE_DIR/bin" "$LOG" "$FIXTURES/$post_live" "$LIVE" "$exec_status"
  set +e
  env PATH="$CASE_DIR/bin:$PATH" \
    DJINN_KUBECTL=kubectl \
    DJINN_K3S_RESTART_CMD='systemctl restart k3s' \
    DJINN_CGROUP_INSTALL_DIR="$CASE_DIR/install" \
    DJINN_CGROUP_LIVE_CONFIG_PATH="$LIVE" \
    "$@" \
    bash "$CONFORMANCE" --node "$NODE" >"$CASE_DIR/stdout" 2>"$CASE_DIR/stderr"
  STATUS=$?
  set -e
}

restarts() { grep -c '^restart ' "$LOG" || true; }

expect_failed() {
  local name=$1
  if [ "$STATUS" -eq 0 ]; then fail "$name: unexpectedly succeeded"; else ok "$name failed as required"; fi
  if [ -s "$CASE_DIR/stdout" ]; then fail "$name: printed on stdout"; fi
  if grep -Fq 'djinn.io/cgroup-writable=true' "$LOG"; then fail "$name: applied the eligibility label"; fi
}

# ---------------------------------------------------------------------------
# 1. The version-3 node this repository could not previously serve.
# ---------------------------------------------------------------------------
run_case v3-success live-v3-vps-preinstall.toml live-v3-vps.toml 0
expect_eq 'v3 success status' 0 "$STATUS"
expect_eq 'v3 success stdout' "$PASS_LINE" "$(cat "$CASE_DIR/stdout")"
expect_eq 'v3 success restarts' 1 "$(restarts)"
if cmp -s "$NODE_DIR/containerd/config-v3.toml.tmpl" "$CASE_DIR/install/config-v3.toml.tmpl"; then
  ok 'v3 installed the version-3 template by its own basename'
else
  fail 'v3 did not install containerd/config-v3.toml.tmpl'
fi
if [ -e "$CASE_DIR/install/config.toml.tmpl" ]; then
  fail 'v3 also wrote the version-2 template filename'
else
  ok 'v3 wrote no version-2 template filename'
fi
if grep -Fxq 'label node fixture-node djinn.io/cgroup-writable=true --overwrite' "$LOG"; then
  ok 'v3 success labelled the node'
else
  fail 'v3 success did not label the node'
fi

# ---------------------------------------------------------------------------
# 2. The version-2 node keeps working, unchanged asset and all.
# ---------------------------------------------------------------------------
run_case v2-success live-v2-k3s-preinstall.toml live-v2-k3s.toml 0
expect_eq 'v2 success status' 0 "$STATUS"
expect_eq 'v2 success stdout' "$PASS_LINE" "$(cat "$CASE_DIR/stdout")"
expect_eq 'v2 success restarts' 1 "$(restarts)"
if cmp -s "$NODE_DIR/containerd/config.toml.tmpl" "$CASE_DIR/install/config.toml.tmpl"; then
  ok 'v2 installed the unchanged version-2 template'
else
  fail 'v2 did not install containerd/config.toml.tmpl'
fi
if [ -e "$CASE_DIR/install/config-v3.toml.tmpl" ]; then
  fail 'v2 also wrote the version-3 template filename'
else
  ok 'v2 wrote no version-3 template filename'
fi

# ---------------------------------------------------------------------------
# 3. Preflight aborts: zero writes and zero restarts.
# ---------------------------------------------------------------------------
assert_aborted_before_any_write() {
  local name=$1 dest=$2
  expect_failed "$name"
  expect_eq "$name restarts" 0 "$(restarts)"
  if [ -e "$dest" ]; then fail "$name: wrote $dest"; else ok "$name wrote no template"; fi
}

run_case unsupported-version detect-version-4.toml live-v3-vps.toml 0
assert_aborted_before_any_write unsupported-version "$CASE_DIR/install/config-v3.toml.tmpl"
if [ -e "$CASE_DIR/install/config.toml.tmpl" ]; then
  fail 'unsupported-version: wrote the version-2 template'
else
  ok 'unsupported-version wrote neither template'
fi

# A version-2 source template on a version-3 node, named so its basename also
# contradicts the detected generation.
run_case source-basename-mismatch live-v3-vps-preinstall.toml live-v3-vps.toml 0 \
  DJINN_CGROUP_TEMPLATE_SOURCE="$NODE_DIR/containerd/config.toml.tmpl"
assert_aborted_before_any_write source-basename-mismatch "$CASE_DIR/install/config-v3.toml.tmpl"

# The same contradiction with a name that asserts nothing: only the template's
# actual namespace can catch it.
mkdir -p "$WORK/neutral"
cp "$NODE_DIR/containerd/config.toml.tmpl" "$WORK/neutral/template.tmpl"
run_case source-content-mismatch live-v3-vps-preinstall.toml live-v3-vps.toml 0 \
  DJINN_CGROUP_TEMPLATE_SOURCE="$WORK/neutral/template.tmpl"
assert_aborted_before_any_write source-content-mismatch "$CASE_DIR/install/config-v3.toml.tmpl"
if grep -Fq 'version-2 runtime table on a version-3 node' "$CASE_DIR/stderr"; then
  ok 'source-content-mismatch names the contradicting namespace'
else
  fail "source-content-mismatch diagnosis: $(cat "$CASE_DIR/stderr")"
fi

# The destination filename k3s would read must match the detected generation.
run_case dest-basename-mismatch live-v3-vps-preinstall.toml live-v3-vps.toml 0 \
  DJINN_CGROUP_TEMPLATE_PATH="$WORK/dest-basename-mismatch-install/config.toml.tmpl"
expect_failed dest-basename-mismatch
expect_eq 'dest-basename-mismatch restarts' 0 "$(restarts)"
if [ -e "$WORK/dest-basename-mismatch-install/config.toml.tmpl" ]; then
  fail 'dest-basename-mismatch: wrote the contradicting destination'
else
  ok 'dest-basename-mismatch wrote no template'
fi

# ---------------------------------------------------------------------------
# 4. The live-table validator, against the real captured production config and
#    four single-line mutations of it.
# ---------------------------------------------------------------------------
assert_restored_after_restart() {
  local name=$1
  expect_failed "$name"
  expect_eq "$name restarts" 2 "$(restarts)"
  if [ -e "$CASE_DIR/install/config-v3.toml.tmpl" ]; then
    fail "$name: left the unproven template installed"
  else
    ok "$name removed the unproven template and restarted again"
  fi
}

run_case validator-cgroup-writable-false live-v3-vps-preinstall.toml live-v3-cgroup-writable-false.toml 0
assert_restored_after_restart validator-cgroup-writable-false
run_case validator-table-deleted live-v3-vps-preinstall.toml live-v3-table-deleted.toml 0
assert_restored_after_restart validator-table-deleted
run_case validator-v2-header live-v3-vps-preinstall.toml live-v3-v2-header.toml 0
assert_restored_after_restart validator-v2-header
run_case validator-runtime-type live-v3-vps-preinstall.toml live-v3-runtime-type-changed.toml 0
assert_restored_after_restart validator-runtime-type

# ---------------------------------------------------------------------------
# 5. A pre-existing template is restored byte-for-byte, and a probe failure
#    after the restart restores just as a validator failure does.
# ---------------------------------------------------------------------------
PREVIOUS="$WORK/previous.tmpl"
printf '# operator template\n{{ template "base" . }}\n# unrelated local edit\n' >"$PREVIOUS"
mkdir -p "$WORK/restore-previous-install"
cp "$PREVIOUS" "$WORK/restore-previous-install/config-v3.toml.tmpl"
run_case restore-previous live-v3-vps-preinstall.toml live-v3-cgroup-writable-false.toml 0 \
  DJINN_CGROUP_TEMPLATE_PATH="$WORK/restore-previous-install/config-v3.toml.tmpl"
expect_failed restore-previous
expect_eq 'restore-previous restarts' 2 "$(restarts)"
if cmp -s "$PREVIOUS" "$WORK/restore-previous-install/config-v3.toml.tmpl"; then
  ok 'restore-previous put the previous template back byte-for-byte'
else
  fail 'restore-previous did not restore the previous template'
fi

run_case probe-failure live-v3-vps-preinstall.toml live-v3-vps.toml 1
assert_restored_after_restart probe-failure
if grep -q '^delete pod ' "$LOG"; then ok 'probe-failure deleted the probe'; else fail 'probe-failure left the probe'; fi
unlabels=$(grep -Fc 'label node fixture-node djinn.io/cgroup-writable- --overwrite' "$LOG" || true)
if [ "$unlabels" -ge 2 ]; then
  ok 'probe-failure removed eligibility again in cleanup'
else
  fail "probe-failure eligibility removals: $unlabels"
fi

# ---------------------------------------------------------------------------
# 6. Fixture mode: the success transcript is exact and every failure case,
#    including the new version-mismatch case, removes eligibility only.
# ---------------------------------------------------------------------------
run_fixture_case() {
  local name=$1 log="$WORK/fixture-$1.log" out status
  : >"$log"
  set +e
  out=$(DJINN_CGROUP_FIXTURE_MODE=1 DJINN_CGROUP_FIXTURE_CASE="$name" DJINN_CGROUP_FIXTURE_LOG="$log" \
    bash "$CONFORMANCE" --node "$NODE" 2>"$WORK/fixture-$1.err")
  status=$?
  set -e
  if [ "$name" = success ]; then
    expect_eq 'fixture success status' 0 "$status"
    expect_eq 'fixture success stdout' "$PASS_LINE" "$out"
  else
    if [ "$status" -eq 0 ]; then fail "fixture $name: unexpectedly succeeded"; else ok "fixture $name failed"; fi
    if [ -n "$out" ]; then fail "fixture $name: printed on stdout"; fi
    if grep -q "^unlabel node=$NODE\$" "$log"; then
      ok "fixture $name removed eligibility"
    else
      fail "fixture $name did not remove eligibility"
    fi
    # A recognised case reaches the modelled failure branch. Without this the
    # unknown-case fallback would satisfy every other assertion here.
    if grep -q "^failure=$name\$" "$log" && grep -q "^cleanup node=$NODE\$" "$log"; then
      ok "fixture $name is a modelled failure case"
    else
      fail "fixture $name did not reach the modelled failure branch: $(tr '\n' ' ' <"$log")"
    fi
    if grep -q "^label node=" "$log"; then fail "fixture $name applied the label"; fi
  fi
}
run_fixture_case success
run_fixture_case timeout
run_fixture_case wrong-node
run_fixture_case sandbox
run_fixture_case isolation
run_fixture_case readonly
run_fixture_case mutation-success
run_fixture_case handler-removed
run_fixture_case version-mismatch

# ---------------------------------------------------------------------------
# 7. Load-bearing text a reviewer will diff, and sole ownership of the label.
# ---------------------------------------------------------------------------
conformance_text=$(cat "$CONFORMANCE")
assert_contains() {
  case "$conformance_text" in
    *"$2"*) ok "preserved: $1" ;;
    *) fail "preserved text lost: $1" ;;
  esac
}
assert_contains 'LAUNCHER_PROBE' "LAUNCHER_PROBE='set -eu"
assert_contains 'launcher private root' '[ ! -d "$root/system.slice" ]'
assert_contains 'launcher retained leaf' 'launcher_leaf="$root/.djinn-launcher-leaf"'
assert_contains 'WORKER_PROBE' "WORKER_PROBE='set -eu"
assert_contains 'worker denial helper' 'must_deny() { if sh -c "$1"; then'
assert_contains 'wait_for_probe' 'wait_for_probe() {'
assert_contains 'verify_node_identity' 'verify_node_identity() {'
pass_format_count=$(grep -Fc \
  "printf 'PASS node=%s handler=runc-cgroupwritable cgroup_root=/ writable=true isolated=true worker_denials=true\\n'" \
  "$CONFORMANCE" || true)
expect_eq 'PASS printf format occurrences' 2 "$pass_format_count"
owners=$(grep -rl 'label node' "$NODE_DIR" --include='*.sh' | grep -v "$SCRIPT_DIR" | sort)
expect_eq 'sole label owner under deploy/node' "$CONFORMANCE" "$owners"

# ---------------------------------------------------------------------------
# 8. Non-vacuity: the pre-change script restarts k3s on the very fixture the
#    preflight now refuses. Skipped once the baseline ref carries the preflight.
# ---------------------------------------------------------------------------
baseline_ref=${DJINN_CONFORMANCE_BASELINE_REF:-origin/main}
baseline_path='deploy/node/k3s/djinn-cgroup-writable-conformance.sh'
if ! command -v git >/dev/null 2>&1 || ! baseline=$(git -C "$REPO_DIR" show "$baseline_ref:$baseline_path" 2>/dev/null); then
  printf 'SKIP non-vacuity: %s:%s is unavailable\n' "$baseline_ref" "$baseline_path"
elif printf '%s' "$baseline" | grep -q 'preflight_version_tuple'; then
  printf 'SKIP non-vacuity: %s already carries the preflight\n' "$baseline_ref"
else
  BASELINE_DIR="$WORK/baseline"
  mkdir -p "$BASELINE_DIR/install"
  printf '%s' "$baseline" >"$BASELINE_DIR/conformance.sh"
  : >"$BASELINE_DIR/log"
  cp "$FIXTURES/live-v3-vps-preinstall.toml" "$BASELINE_DIR/live.toml"
  make_stubs "$BASELINE_DIR/bin" "$BASELINE_DIR/log" "$FIXTURES/live-v3-vps.toml" "$BASELINE_DIR/live.toml" 0
  set +e
  env PATH="$BASELINE_DIR/bin:$PATH" \
    DJINN_KUBECTL=kubectl \
    DJINN_K3S_RESTART_CMD='systemctl restart k3s' \
    DJINN_CGROUP_TEMPLATE_SOURCE="$NODE_DIR/containerd/config.toml.tmpl" \
    DJINN_CGROUP_TEMPLATE_PATH="$BASELINE_DIR/install/config.toml.tmpl" \
    DJINN_CGROUP_LIVE_CONFIG_PATH="$BASELINE_DIR/live.toml" \
    bash "$BASELINE_DIR/conformance.sh" --node "$NODE" >/dev/null 2>&1
  set -e
  baseline_restarts=$(grep -c '^restart ' "$BASELINE_DIR/log" || true)
  if [ "$baseline_restarts" -ge 1 ]; then
    ok "non-vacuity: $baseline_ref restarted k3s $baseline_restarts time(s) on the refused fixture"
  else
    fail "non-vacuity: $baseline_ref recorded no restart, so the new assertions prove nothing"
  fi
  if [ -e "$BASELINE_DIR/install/config.toml.tmpl" ]; then
    ok "non-vacuity: $baseline_ref installed the version-2 template on the version-3 fixture"
  else
    fail "non-vacuity: $baseline_ref installed nothing"
  fi
fi

if [ "$failures" -ne 0 ]; then
  printf 'FAIL: %s conformance version lifecycle assertion(s)\n' "$failures" >&2
  exit 1
fi
echo 'PASS: conformance version preflight, restore and validator contract'
