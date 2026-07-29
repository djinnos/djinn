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
  local dir=$1 log=$2 post_live=$3 live=$4 exec_status=$5 manifest=${6:-/dev/null}
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
  # The `get` arms render jsonpath selectors the way the real apiserver does.
  # Crucially, a selector naming a field the Pod API does not have renders
  # EMPTY rather than a node name, so a comparison against a nonexistent field
  # fails here for exactly the reason it failed on the production node.
  # Per-case identity observations come from the environment:
  #   DJINN_FIXTURE_POD_NODE_NAME       .spec.nodeName on the probe
  #   DJINN_FIXTURE_POD_HOST_IP         .status.hostIP on the probe
  #   DJINN_FIXTURE_NODE_INTERNAL_IPS   the node's InternalIP addresses
  cat >"$dir/kubectl" <<EOF
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "\$*" >>'$log'
selector=\${*: -1}
pod_node_name=\${DJINN_FIXTURE_POD_NODE_NAME-$NODE}
pod_host_ip=\${DJINN_FIXTURE_POD_HOST_IP-10.10.0.7}
node_internal_ips=\${DJINN_FIXTURE_NODE_INTERNAL_IPS-10.10.0.7}
case "\$1" in
  label) exit 0 ;;
  get)
    if [ "\$2" = node ]; then
      case "\$selector" in
        # A {range} over InternalIP addresses emits one address per line.
        *status.addresses*)
          for address in \$node_internal_ips; do printf '%s\n' "\$address"; done ;;
        # The eligibility label read must render empty: this program removed
        # the label before anything else, and ensure_unlabeled proves it.
        *) : ;;
      esac
      exit 0
    fi
    if [ "\$2" = pod ]; then
      if [ "\$selector" = json ]; then exit 0; fi
      rendered=\${selector#jsonpath=}
      rendered=\${rendered//\\{.spec.nodeName\\}/\$pod_node_name}
      rendered=\${rendered//\\{.status.hostIP\\}/\$pod_host_ip}
      # PodStatus has no nodeName field. The apiserver renders it as nothing.
      rendered=\${rendered//\\{.status.nodeName\\}/}
      printf '%s' "\$rendered"
      exit 0
    fi
    ;;
  apply) cat >'$manifest'; exit 0 ;;
  wait) exit 0 ;;
  # The probe emits on BOTH streams, exactly as \`sh -ceux\` does: assertions on
  # stdout, the xtrace on stderr. A passing run must forward neither, so the
  # PASS line stays the whole of stdout; a failing run must forward both, or the
  # operator is told only that conformance failed and has to re-run the exec by
  # hand to learn which assertion never passed.
  exec)
    printf '%s\n' 'probe-emitted-on-stdout'
    printf '%s\n' 'probe-xtrace: + [ 0 1000 = 1000 ]' >&2
    exit $exec_status ;;
  delete) exit 0 ;;
esac
exit 0
EOF
  chmod +x "$dir/id" "$dir/systemctl" "$dir/kubectl"
}

# Every case is described by the live configuration the node starts with, the
# one k3s renders after a restart, and the template wiring under test.
CASE_DIR='' STATUS=0 LOG='' LIVE='' DEST='' MANIFEST=''
run_case() {
  local name=$1 pre_live=$2 post_live=$3 exec_status=$4
  shift 4
  CASE_DIR="$WORK/$name"
  LOG="$CASE_DIR/log"
  LIVE="$CASE_DIR/live.toml"
  MANIFEST="$CASE_DIR/manifest.yaml"
  mkdir -p "$CASE_DIR/install"
  : >"$LOG"
  : >"$MANIFEST"
  cp "$FIXTURES/$pre_live" "$LIVE"
  make_stubs "$CASE_DIR/bin" "$LOG" "$FIXTURES/$post_live" "$LIVE" "$exec_status" "$MANIFEST"
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

# The applied probe manifest decides whether this program can observe a node
# that is not yet eligible. `djinn-cgroup-writable` carries
# `scheduling.nodeSelector: {djinn.io/cgroup-writable: "true"}`, which the
# RuntimeClass admission controller merges into any Pod naming it; the kubelet
# then evaluates that as a NodeAffinity predicate and rejects the Pod, even
# though `spec.nodeName` already bypassed the scheduler. The probe must name
# the unconstrained `djinn-cgroup-writable-probe` class and stay node-bound.
# Exact-line matching is required: the task-run class name is a PREFIX of the
# probe class name.
assert_probe_manifest() {
  local name=$1
  if grep -Fxq '  runtimeClassName: djinn-cgroup-writable-probe' "$MANIFEST"; then
    ok "$name probe names the unconstrained probe RuntimeClass"
  else
    fail "$name probe does not name djinn-cgroup-writable-probe: [$(grep -F 'runtimeClassName' "$MANIFEST" || true)]"
  fi
  if grep -Fxq '  runtimeClassName: djinn-cgroup-writable' "$MANIFEST"; then
    fail "$name probe names the node-selected task-run RuntimeClass, which the kubelet rejects on an unlabeled node"
  else
    ok "$name probe avoids the node-selected task-run RuntimeClass"
  fi
  if grep -Fxq "  nodeName: $NODE" "$MANIFEST"; then
    ok "$name probe is bound to the exact node by spec.nodeName"
  else
    fail "$name probe is not bound with spec.nodeName"
  fi
}

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
assert_probe_manifest v3
# The identity proof must actually be performed on the passing path, otherwise
# section 6's rejections would be guarding a check nothing ever reaches.
if grep -Fq 'get pod' "$LOG" && grep -Fq 'jsonpath={.status.hostIP}' "$LOG"; then
  ok 'v3 success read the host the kubelet reported for the probe'
else
  fail 'v3 success never read the probe status.hostIP'
fi
if grep -Fq '{range .status.addresses[?(@.type=="InternalIP")]}' "$LOG"; then
  ok "v3 success cross-checked the node's own InternalIP"
else
  fail 'v3 success never read the node InternalIP'
fi
# The PASS-line contract: a successful run forwards nothing the probe wrote on
# either stream, so capturing the transcript for the failure path cannot change
# what an operator or an automation parses out of a passing run.
if grep -Fq 'probe-emitted-on-stdout' "$CASE_DIR/stdout" || grep -Fq 'probe-xtrace' "$CASE_DIR/stdout"; then
  fail "v3 success leaked probe output onto stdout: $(cat "$CASE_DIR/stdout")"
else
  ok 'v3 success keeps the PASS line as the whole of stdout'
fi
if [ -s "$CASE_DIR/stderr" ]; then
  fail "v3 success wrote to stderr: $(cat "$CASE_DIR/stderr")"
else
  ok 'v3 success forwards no probe transcript on stderr either'
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
assert_probe_manifest v2

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
# A failing probe must name the assertion that failed. Suppressing the exec with
# `>/dev/null` is why three defects in this program stayed invisible: the
# operator saw only the summary line and had to re-run the exec by hand under
# `set -x` to find the assertion that had never once passed.
for stream_marker in 'probe-xtrace: + [ 0 1000 = 1000 ]' 'probe-emitted-on-stdout'; do
  if grep -Fq "$stream_marker" "$CASE_DIR/stderr"; then
    ok "probe-failure surfaced the probe transcript [$stream_marker]"
  else
    fail "probe-failure suppressed [$stream_marker]: $(cat "$CASE_DIR/stderr")"
  fi
done
if grep -Fq 'launcher/worker cgroup conformance failed' "$CASE_DIR/stderr"; then
  ok 'probe-failure still reports the summary diagnosis'
else
  fail "probe-failure lost its summary diagnosis: $(cat "$CASE_DIR/stderr")"
fi
unlabels=$(grep -Fc 'label node fixture-node djinn.io/cgroup-writable- --overwrite' "$LOG" || true)
if [ "$unlabels" -ge 2 ]; then
  ok 'probe-failure removed eligibility again in cleanup'
else
  fail "probe-failure eligibility removals: $unlabels"
fi

# ---------------------------------------------------------------------------
# 6. Node identity. The check must prove the probe RAN on the requested node,
#    not merely that it was requested there. PodStatus has no `nodeName`
#    field, so the only available cross-check is the admitting kubelet's
#    reported `.status.hostIP` against the Node object's own InternalIP.
#    Every empty observation must fail closed: an empty-vs-empty comparison
#    that passes is precisely the defect these cases exist to forbid.
# ---------------------------------------------------------------------------
assert_identity_rejected() {
  local name=$1
  shift
  assert_restored_after_restart "$name"
  if grep -Fq 'probe node identity mismatch' "$CASE_DIR/stderr"; then
    ok "$name reports a node identity mismatch"
  else
    fail "$name diagnosis: $(cat "$CASE_DIR/stderr")"
  fi
  local needle
  for needle in "$@"; do
    if grep -Fq "$needle" "$CASE_DIR/stderr"; then
      ok "$name reports the observed value [$needle]"
    else
      fail "$name omitted [$needle] from: $(cat "$CASE_DIR/stderr")"
    fi
  done
  # An unproven host must abort before any cgroup authorization observation.
  if grep -q '^exec ' "$LOG"; then
    fail "$name ran the cgroup probe on an unproven host"
  else
    ok "$name aborted before the cgroup probe"
  fi
}

# The real production node publishes one InternalIP per address family while
# the kubelet writes only the primary into status.hostIP. Whole-string equality
# against the node's address list would reject this healthy node outright.
run_case identity-dual-stack live-v3-vps-preinstall.toml live-v3-vps.toml 0 \
  DJINN_FIXTURE_NODE_INTERNAL_IPS='10.10.0.7 fd00::7' \
  DJINN_FIXTURE_POD_HOST_IP=10.10.0.7
expect_eq 'identity-dual-stack status' 0 "$STATUS"
expect_eq 'identity-dual-stack stdout' "$PASS_LINE" "$(cat "$CASE_DIR/stdout")"

# The node lookup renders empty: no InternalIP is published at all.
run_case identity-node-ip-empty live-v3-vps-preinstall.toml live-v3-vps.toml 0 \
  DJINN_FIXTURE_NODE_INTERNAL_IPS=
assert_identity_rejected identity-node-ip-empty \
  'publishes no InternalIP address' 'status.hostIP=[10.10.0.7]'

# The kubelet never reported a host for the probe.
run_case identity-host-ip-empty live-v3-vps-preinstall.toml live-v3-vps.toml 0 \
  DJINN_FIXTURE_POD_HOST_IP=
assert_identity_rejected identity-host-ip-empty \
  'status.hostIP is empty' 'requested node=[fixture-node]'

# BOTH sides empty. A comparison of one empty rendering against another must
# never satisfy the check; this is the exact hole `{.status.nodeName}` opened.
run_case identity-both-empty live-v3-vps-preinstall.toml live-v3-vps.toml 0 \
  DJINN_FIXTURE_POD_HOST_IP= DJINN_FIXTURE_NODE_INTERNAL_IPS=
assert_identity_rejected identity-both-empty 'status.hostIP is empty'

# A kubelet on some other host reported this probe.
run_case identity-foreign-host-ip live-v3-vps-preinstall.toml live-v3-vps.toml 0 \
  DJINN_FIXTURE_POD_HOST_IP=10.10.0.9
assert_identity_rejected identity-foreign-host-ip \
  'status.hostIP=[10.10.0.9]' 'node InternalIPs=[10.10.0.7]'

# Address membership is compared whole. A substring test would accept this,
# because 10.10.0.7 is a substring of the node's only address 10.10.0.71.
run_case identity-substring-near-miss live-v3-vps-preinstall.toml live-v3-vps.toml 0 \
  DJINN_FIXTURE_POD_HOST_IP=10.10.0.7 DJINN_FIXTURE_NODE_INTERNAL_IPS=10.10.0.71
assert_identity_rejected identity-substring-near-miss \
  'status.hostIP=[10.10.0.7]' 'node InternalIPs=[10.10.0.71]'

# The binding half still has to hold on its own.
run_case identity-bound-elsewhere live-v3-vps-preinstall.toml live-v3-vps.toml 0 \
  DJINN_FIXTURE_POD_NODE_NAME=other-node
assert_identity_rejected identity-bound-elsewhere \
  'spec.nodeName=[other-node]' 'requested node=[fixture-node]'

# ---------------------------------------------------------------------------
# 7. Worker identity: the supplementary group SET. The launcher phase runs as
#    `runAsUser: 0`, so its own supplementary set is `0 1000` — root's group 0
#    plus the pod's fsGroup. No rendered task-run worker ever has that set:
#    worker_security_context() in server/crates/djinn-k8s/src/launcher.rs pins
#    the worker container to runAsUser/runAsGroup 1000 with runAsNonRoot and the
#    pod securityContext there to fsGroup 1000 (asserted in
#    server/crates/djinn-k8s/src/job.rs), so a real worker's supplementary set
#    is exactly `1000` and nothing else. The probe must
#    therefore STATE the worker group set, never inherit the launcher's, and the
#    assertion must reject the inherited shape rather than tolerate it.
# ---------------------------------------------------------------------------
setpriv_line=$(grep -F 'exec setpriv ' "$CONFORMANCE" || true)
if [ -z "$setpriv_line" ]; then
  fail 'no setpriv launcher-to-worker transition found in the conformance script'
fi
case "$setpriv_line" in
  *--groups=1000*)
    ok 'worker transition states the supplementary group set explicitly' ;;
  *)
    fail "worker transition does not set --groups=1000: [$setpriv_line]" ;;
esac
# setpriv rejects --groups alongside --keep-groups/--clear-groups/--init-groups,
# so this is not merely redundant with the assertion above: --keep-groups is the
# defect, and it is the one shape that would silently reintroduce group 0.
case "$setpriv_line" in
  *--keep-groups*)
    fail "worker transition uses --keep-groups, which carries the launcher's group 0 into the worker identity: [$setpriv_line]" ;;
  *)
    ok 'worker transition never inherits the launcher supplementary groups' ;;
esac

# The assertion itself, executed. The line is lifted verbatim out of
# WORKER_PROBE (un-escaping the '\'' sequences bash uses to embed a single quote
# inside a single-quoted literal) and pointed at a fixture status file, so these
# cases exercise the same expression the worker phase runs on the node — it
# cannot drift from the script without this section noticing.
groups_assertion=$(grep -F '/^Groups:/' "$CONFORMANCE" || true)
if [ -z "$groups_assertion" ]; then
  fail 'WORKER_PROBE carries no Groups: assertion at all'
else
  embedded_quote="'\\''"
  bare_quote="'"
  groups_assertion=${groups_assertion//"$embedded_quote"/"$bare_quote"}
  assert_groups_fixture() {
    local name=$1 status_line=$2 expect=$3 fixture snippet observed
    fixture="$WORK/status-$name"
    # Shaped exactly like the kernel's /proc/<pid>/status rendering: a tab after
    # the key, a space after every GID including the last.
    printf 'Name:\tsh\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\n%s\nNoNewPrivs:\t1\n' \
      "$status_line" >"$fixture"
    snippet=${groups_assertion//\/proc\/self\/status/$fixture}
    set +e
    sh -eu -c "$snippet" >/dev/null 2>&1
    observed=$?
    set -e
    if [ "$expect" = pass ] && [ "$observed" -eq 0 ]; then
      ok "worker Groups assertion accepts the rendered worker set [$status_line]"
    elif [ "$expect" = fail ] && [ "$observed" -ne 0 ]; then
      ok "worker Groups assertion rejects [$status_line]"
    else
      fail "worker Groups assertion on [$status_line]: expected to $expect, exit=$observed"
    fi
  }
  # The set a rendered worker actually has, trailing space included.
  assert_groups_fixture rendered-worker "$(printf 'Groups:\t1000 ')" pass
  # What --keep-groups produced on the production node: root's group 0 leaked
  # through the transition and the worker ran with `0 1000`. This must FAIL.
  # Relaxing it would make the probe certify a worker identity production never
  # has, which is the entire class of defect this program exists to exclude.
  assert_groups_fixture leaked-root-group "$(printf 'Groups:\t0 1000 ')" fail
  # Neither may a bare root group, a reordering, nor an empty set pass.
  assert_groups_fixture only-root-group "$(printf 'Groups:\t0 ')" fail
  assert_groups_fixture reordered-with-root "$(printf 'Groups:\t1000 0 ')" fail
  assert_groups_fixture empty-group-set "$(printf 'Groups:\t')" fail
fi

# The load-bearing property of --groups is that it REPLACES the set by
# setgroups(2) and cannot append to it. setpriv enforces that by refusing
# --groups alongside --keep-groups at all, which is observable without any
# privilege. The diagnostic wording differs between util-linux releases, so only
# the refusal is asserted.
if command -v setpriv >/dev/null 2>&1; then
  set +e
  setpriv_exclusivity=$(setpriv --groups=1000 --keep-groups /bin/true 2>&1)
  setpriv_exclusivity_status=$?
  set -e
  if [ "$setpriv_exclusivity_status" -ne 0 ] && [ -n "$setpriv_exclusivity" ]; then
    ok "setpriv refuses --groups alongside --keep-groups, so --groups cannot append [$setpriv_exclusivity]"
  else
    fail "setpriv accepted --groups with --keep-groups (exit=$setpriv_exclusivity_status): --groups may not be replacing the set"
  fi
else
  printf 'SKIP setpriv group-flag exclusivity: setpriv is not installed\n'
fi

# ---------------------------------------------------------------------------
# 8. CPU controller delegation. A child of the delegated root is born with the
#    core interface files only. `cpu.max` exists in it exactly when the parent
#    already enables the cpu controller in its own `cgroup.subtree_control`. The
#    launcher phase never performed that write, so on the production node
#    `.djinn-launcher-leaf` came up with an empty `cgroup.controllers` and no
#    `cpu.max` at all; the worker phase then aborted outright at
#    `launcher_cpu_max=$(cat "$launcher_leaf/cpu.max")` under `set -eu`, and the
#    leaf-`cpu.max` denial it was walking towards could only ever have failed
#    with ENOENT — a failure that says nothing about the worker's authority,
#    because a write to a missing path fails for root too.
#
#    The launcher phase is lifted verbatim out of the script and executed against
#    a fake delegated root with PATH shims that model the kernel behaviours it
#    depends on, so these cases cannot drift from the shipped text.
# ---------------------------------------------------------------------------
EMBEDDED_QUOTE="'\\''"
BARE_QUOTE="'"

# Lift one single-quoted probe payload out of the script and undo the '\''
# sequences bash uses to embed a single quote inside a single-quoted literal.
extract_probe_payload() {
  local name=$1 line collecting=0 out=''
  while IFS= read -r line; do
    if [ "$collecting" -eq 0 ]; then
      case "$line" in
        "$name=$BARE_QUOTE"*) collecting=1; line=${line#"$name=$BARE_QUOTE"} ;;
        *) continue ;;
      esac
    fi
    case "$line" in
      # A line ending in an embedded quote is not the end of the literal.
      *"$EMBEDDED_QUOTE") out="${out}${line}"$'\n' ;;
      *"$BARE_QUOTE") out="${out}${line%"$BARE_QUOTE"}"$'\n'; break ;;
      *) out="${out}${line}"$'\n' ;;
    esac
  done <"$CONFORMANCE"
  printf '%s' "${out//"$EMBEDDED_QUOTE"/"$BARE_QUOTE"}"
}

# Three kernel behaviours the launcher phase depends on, and nothing else:
#   * `stat -fc %T` on a cgroup2 mount reports cgroup2fs;
#   * `cgroup.subtree_control` renders the ENABLED set rather than the directive
#     last written into it, so `printf +cpu >` reads back as `cpu`;
#   * a child cgroup is born with the core interface files, and gains the cpu
#     controller's own files — `cpu.max` among them — only when its parent
#     already enables cpu. Both file lists are the ones observed on the node.
#     Its interface files are not dirents an `rmdir` has to clear first, so a
#     childless, unpopulated cgroup is removable exactly as kernfs allows.
# `cgroup.procs` of the delegated root additionally loses every pid migrated
# into a descendant, which is what makes vacating the root observable at all.
make_cgroup_shims() {
  local dir=$1 root=$2 real_cat real_mkdir real_rmdir real_stat
  real_cat=$(command -v cat)
  real_mkdir=$(command -v mkdir)
  real_rmdir=$(command -v rmdir)
  real_stat=$(command -v stat)
  mkdir -p "$dir"
  cat >"$dir/stat" <<EOF
#!/usr/bin/env bash
if [ "\${1-}" = -fc ] && [ "\${2-}" = %T ]; then echo cgroup2fs; exit 0; fi
exec '$real_stat' "\$@"
EOF
  cat >"$dir/cat" <<EOF
#!/usr/bin/env bash
set -uo pipefail
root='$root'
case "\${1-}" in
  */cgroup.subtree_control)
    enabled=''
    for token in \$('$real_cat' "\$1"); do
      case "\$token" in
        -*) : ;;
        *) enabled="\${enabled:+\$enabled }\${token#+}" ;;
      esac
    done
    [ -z "\$enabled" ] || printf '%s\n' "\$enabled"
    exit 0 ;;
  "\$root/cgroup.procs")
    migrated=\$('$real_cat' "\$root"/*/cgroup.procs 2>/dev/null || true)
    for pid in \$('$real_cat' "\$1"); do
      case " \$migrated " in *" \$pid "*) ;; *) printf '%s\n' "\$pid" ;; esac
    done
    exit 0 ;;
esac
exec '$real_cat' "\$@"
EOF
  cat >"$dir/mkdir" <<EOF
#!/usr/bin/env bash
set -uo pipefail
target=\${1-}
'$real_mkdir' "\$target"
for file in cgroup.controllers cgroup.events cgroup.freeze cgroup.kill \\
  cgroup.max.depth cgroup.max.descendants cgroup.pressure cgroup.procs \\
  cgroup.stat cgroup.subtree_control cgroup.threads cgroup.type \\
  io.pressure memory.pressure; do
  : >"\$target/\$file"
done
printf 'domain\n' >"\$target/cgroup.type"
case " \$('$dir/cat' "\$(dirname "\$target")/cgroup.subtree_control") " in
  *" cpu "*)
    printf 'cpu\n' >"\$target/cgroup.controllers"
    for file in cpu.idle cpu.max.burst cpu.pressure cpu.stat cpu.stat.local \\
      cpu.uclamp.max cpu.uclamp.min cpu.weight cpu.weight.nice; do
      : >"\$target/\$file"
    done
    printf 'max 100000\n' >"\$target/cpu.max" ;;
esac
EOF
  cat >"$dir/rmdir" <<EOF
#!/usr/bin/env bash
set -uo pipefail
target=\${1-}
# A cgroup with descendant directories, or with processes still in it, must not
# be removable; its own interface files must not stand in the way.
if [ -n "\$(find "\$target" -mindepth 1 -maxdepth 1 -type d -print -quit)" ]; then
  printf 'rmdir: failed to remove %s: Device or resource busy\n' "\$target" >&2
  exit 1
fi
if [ -s "\$target/cgroup.procs" ]; then
  printf 'rmdir: failed to remove %s: Device or resource busy\n' "\$target" >&2
  exit 1
fi
find "\$target" -mindepth 1 -maxdepth 1 -type f -delete
exec '$real_rmdir' "\$target"
EOF
  chmod +x "$dir/stat" "$dir/cat" "$dir/mkdir" "$dir/rmdir"
}

LAUNCHER_ROOT='' LAUNCHER_STATUS=0 LAUNCHER_ERR=''
run_launcher_phase() {
  local name=$1 payload=$2 dir proc
  dir="$WORK/cgroup-$name"
  LAUNCHER_ROOT="$dir/root"
  LAUNCHER_ERR="$dir/err"
  proc="$dir/proc-self-cgroup"
  rm -rf "$dir"
  mkdir -p "$LAUNCHER_ROOT"
  printf 'cpuset cpu io memory hugetlb pids rdma misc\n' >"$LAUNCHER_ROOT/cgroup.controllers"
  : >"$LAUNCHER_ROOT/cgroup.subtree_control"
  printf 'domain\n' >"$LAUNCHER_ROOT/cgroup.type"
  printf 'max 100000\n' >"$LAUNCHER_ROOT/cpu.max"
  # One occupant: a shell `>` redirect truncates, while the kernel ignores the
  # offset on cgroup.procs and takes one pid per write.
  printf '4242\n' >"$LAUNCHER_ROOT/cgroup.procs"
  printf '0::/\n' >"$proc"
  make_cgroup_shims "$dir/bin" "$LAUNCHER_ROOT"
  payload=${payload//root=\/sys\/fs\/cgroup/root=$LAUNCHER_ROOT}
  payload=${payload//\/proc\/self\/cgroup/$proc}
  set +e
  env PATH="$dir/bin:$PATH" sh -eu -c "$payload" >"$dir/out" 2>"$LAUNCHER_ERR"
  LAUNCHER_STATUS=$?
  set -e
}

launcher_payload=$(extract_probe_payload LAUNCHER_PROBE)
worker_payload=$(extract_probe_payload WORKER_PROBE)
delegate_write='printf +cpu > "$root/cgroup.subtree_control"'
leaf_cpu_max_guard='[ -f "$launcher_leaf/cpu.max" ]'
worker_leaf_read='launcher_cpu_max=$(cat "$launcher_leaf/cpu.max")'

if [ -z "$launcher_payload" ] || [ -z "$worker_payload" ]; then
  fail 'could not lift the launcher/worker probe payloads out of the conformance script'
else
  # The delegation write must exist, and must precede the leaf it enables. A
  # leaf created first is born without cpu.max and stays that way.
  delegate_line=$(printf '%s\n' "$launcher_payload" | grep -Fn "$delegate_write" | cut -d: -f1 | head -1)
  leaf_line=$(printf '%s\n' "$launcher_payload" | grep -Fn 'mkdir "$launcher_leaf"' | cut -d: -f1 | head -1)
  if [ -z "$delegate_line" ]; then
    fail 'the launcher phase never writes +cpu to the delegated root cgroup.subtree_control'
  elif [ -z "$leaf_line" ]; then
    fail 'the launcher phase never creates the retained launcher leaf'
  elif [ "$delegate_line" -lt "$leaf_line" ]; then
    ok 'launcher phase delegates the cpu controller before creating the launcher leaf'
  else
    fail "launcher phase creates the leaf (line $leaf_line) before delegating cpu (line $delegate_line)"
  fi

  # The sequence mirrors Bootstrap in the launcher crate. The holding-leaf name
  # is that crate's INIT_LEAF constant, not a name invented here.
  BOOTSTRAP_RS="$REPO_DIR/server/crates/djinn-cgroup-launcher/src/bootstrap.rs"
  if [ -r "$BOOTSTRAP_RS" ]; then
    init_leaf_name=$(sed -n 's/.*INIT_LEAF: &str = "\([^"]*\)".*/\1/p' "$BOOTSTRAP_RS" | head -1)
    if [ -z "$init_leaf_name" ]; then
      fail 'could not read INIT_LEAF out of the launcher bootstrap'
    elif printf '%s\n' "$launcher_payload" | grep -Fq "init_leaf=\"\$root/$init_leaf_name\""; then
      ok "launcher phase vacates the root into the crate's own INIT_LEAF [$init_leaf_name]"
    else
      fail "launcher phase does not use the crate's INIT_LEAF [$init_leaf_name]"
    fi
    if printf '%s\n' "$worker_payload" | grep -Fq "/proc/self/cgroup)\" = /$init_leaf_name ]"; then
      ok "worker phase asserts it now sits in the holding leaf [/$init_leaf_name]"
    else
      fail "worker phase does not assert the post-bootstrap cgroup path [/$init_leaf_name]"
    fi
  else
    printf 'SKIP launcher INIT_LEAF cross-check: %s is unavailable\n' "$BOOTSTRAP_RS"
  fi

  # The denial must be aimed at a file the launcher phase proved into existence.
  if printf '%s\n' "$worker_payload" | grep -Fq "$leaf_cpu_max_guard"; then
    ok 'worker phase requires the leaf cpu.max to EXIST before trying to write it'
  else
    fail 'worker phase writes the leaf cpu.max without first proving the file exists'
  fi

  # (a) The shipped launcher phase, executed.
  run_launcher_phase shipped "$launcher_payload"
  expect_eq 'shipped launcher phase status' 0 "$LAUNCHER_STATUS"
  if [ "$LAUNCHER_STATUS" -ne 0 ]; then
    fail "shipped launcher phase transcript: $(cat "$LAUNCHER_ERR")"
  fi
  if grep -Fq cpu "$LAUNCHER_ROOT/cgroup.subtree_control"; then
    ok 'shipped launcher phase enabled cpu in the delegated root cgroup.subtree_control'
  else
    fail 'shipped launcher phase left cgroup.subtree_control empty'
  fi
  if [ -d "$LAUNCHER_ROOT/init" ] && [ -s "$LAUNCHER_ROOT/init/cgroup.procs" ]; then
    ok 'shipped launcher phase vacated the delegated root into the holding leaf'
  else
    fail 'shipped launcher phase left the delegated root occupied'
  fi
  SHIPPED_LEAF="$LAUNCHER_ROOT/.djinn-launcher-leaf"
  if [ -f "$SHIPPED_LEAF/cpu.max" ]; then
    ok 'shipped launcher phase produced a launcher leaf that HAS a cpu.max'
  else
    fail 'shipped launcher phase produced a launcher leaf with no cpu.max'
  fi
  # The measured node values: the birth clamp, then the lift over it.
  expect_eq 'shipped launcher leaf cpu.max after the lift' '400000 100000' \
    "$(cat "$SHIPPED_LEAF/cpu.max" 2>/dev/null || true)"

  # (b) Non-vacuity: delete just the delegation write and the phase must FAIL.
  no_delegate=$(printf '%s\n' "$launcher_payload" | grep -Fv "$delegate_write")
  if [ "$(printf '%s\n' "$no_delegate" | wc -l)" -eq "$(printf '%s\n' "$launcher_payload" | wc -l)" ]; then
    fail 'the delegation write was not removable, so the non-vacuity case proves nothing'
  fi
  run_launcher_phase no-delegate "$no_delegate"
  if [ "$LAUNCHER_STATUS" -ne 0 ]; then
    ok 'a launcher phase that skips the delegation write FAILS'
  else
    fail 'a launcher phase that skips the delegation write still passed'
  fi

  # (c) The pre-fix launcher phase: no delegation write, no readback of it and
  #     no cpu.max guard. It succeeds, and the leaf it leaves behind has no
  #     cpu.max — which is exactly the node state that aborted the worker.
  prefix_payload=$(printf '%s\n' "$launcher_payload" |
    grep -Fv -e "$delegate_write" -e 'cgroup.subtree_control")" = cpu ]' \
      -e "$leaf_cpu_max_guard" -e '25000 100000' -e '400000 100000')
  run_launcher_phase prefix "$prefix_payload"
  expect_eq 'pre-fix launcher phase status' 0 "$LAUNCHER_STATUS"
  PREFIX_LEAF="$LAUNCHER_ROOT/.djinn-launcher-leaf"
  if [ -d "$PREFIX_LEAF" ] && [ ! -e "$PREFIX_LEAF/cpu.max" ]; then
    ok 'pre-fix launcher phase leaves a launcher leaf with no cpu.max at all'
  else
    fail 'pre-fix launcher phase did not reproduce the leaf-without-cpu.max state'
  fi
  if [ -s "$PREFIX_LEAF/cgroup.controllers" ]; then
    fail "pre-fix leaf cgroup.controllers is not empty: [$(cat "$PREFIX_LEAF/cgroup.controllers")]"
  else
    ok 'pre-fix leaf cgroup.controllers is empty, as observed on the node'
  fi

  # (d) The vacuity proof for the denial, using the worker's own hard read
  #     against each tree. Against the pre-fix leaf it aborts with ENOENT under
  #     `set -eu` — the write that follows could never have been an
  #     authorization observation. Against the shipped leaf the file exists, so
  #     the denial that follows can only be about authority.
  assert_worker_leaf_read() {
    local name=$1 leaf=$2 expect=$3 out status
    set +e
    out=$(sh -eu -c "launcher_leaf='$leaf'
$worker_leaf_read
[ -n \"\$launcher_cpu_max\" ]" 2>&1)
    status=$?
    set -e
    if [ "$expect" = pass ] && [ "$status" -eq 0 ]; then
      ok "worker leaf cpu.max read succeeds against $name"
    elif [ "$expect" = abort ] && [ "$status" -ne 0 ]; then
      case "$out" in
        *'No such file or directory'*)
          ok "worker leaf cpu.max read aborts against $name [$out]" ;;
        *)
          fail "worker leaf cpu.max read against $name failed for another reason: [$out]" ;;
      esac
    else
      fail "worker leaf cpu.max read against $name: expected to $expect, exit=$status [$out]"
    fi
  }
  assert_worker_leaf_read 'the pre-fix leaf' "$PREFIX_LEAF" abort
  assert_worker_leaf_read 'the shipped leaf' "$SHIPPED_LEAF" pass
fi

# ---------------------------------------------------------------------------
# 9. Fixture mode: the success transcript is exact and every failure case,
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
# 10. Load-bearing text a reviewer will diff, and sole ownership of the label.
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
assert_contains 'launcher vacates the delegated root' 'init_leaf="$root/init"'
assert_contains 'launcher delegates the cpu controller' 'printf +cpu > "$root/cgroup.subtree_control"'
assert_contains 'launcher proves the leaf has a cpu.max' '[ -f "$launcher_leaf/cpu.max" ]'
assert_contains 'launcher writes the measured birth clamp' '25000 100000'
assert_contains 'launcher lifts the measured clamp' '400000 100000'
assert_contains 'WORKER_PROBE' "WORKER_PROBE='set -eu"
assert_contains 'worker denial helper' 'must_deny() { if sh -c "$1"; then'
assert_contains 'worker cannot revoke the delegation' \
  'must_deny "printf %s -cpu > \"$root/cgroup.subtree_control\""'
assert_contains 'worker cannot extend the delegation' \
  'must_deny "printf %s +memory > \"$root/cgroup.subtree_control\""'
assert_contains 'wait_for_probe' 'wait_for_probe() {'
assert_contains 'verify_node_identity' 'verify_node_identity() {'
# Both halves of the identity proof, by the fields that actually exist.
assert_contains 'identity reads the probe binding' 'jsonpath={.spec.nodeName}'
assert_contains 'identity reads the reported host' 'jsonpath={.status.hostIP}'
assert_contains 'identity cross-checks the node InternalIP' \
  '{range .status.addresses[?(@.type=="InternalIP")]}'
# The regression guard for this whole change: PodStatus has no nodeName field,
# so any selector naming one renders empty and can never prove a thing.
case "$conformance_text" in
  *'.status.nodeName'*)
    fail 'conformance reads .status.nodeName, a field the Pod API does not have' ;;
  *) ok 'preserved: conformance never reads the nonexistent .status.nodeName' ;;
esac
# The probe transcript must stay captured. Discarding the exec is what hid the
# three preceding defects in this program from every operator who ran it.
case "$conformance_text" in
  *'"$WORKER_PROBE" >/dev/null'*)
    fail 'conformance discards the probe transcript again' ;;
  *) ok 'preserved: the probe transcript is captured, not discarded' ;;
esac
assert_contains 'probe transcript reaches stderr on failure' "printf '%s\\n' \"\$probe_transcript\" >&2"
assert_contains 'both probe phases run under xtrace' '/bin/sh -ceux'
assert_contains 'probe-only RuntimeClass' "PROBE_RUNTIME_CLASS='djinn-cgroup-writable-probe'"
assert_contains 'probe manifest uses the probe-only class' 'runtimeClassName: $PROBE_RUNTIME_CLASS'
assert_contains 'probe stays node-bound' 'nodeName: $NODE_NAME'
case "$conformance_text" in
  *"RUNTIME_CLASS='djinn-cgroup-writable'"*)
    fail 'conformance reintroduced the node-selected task-run RuntimeClass for the probe' ;;
  *) ok 'preserved: probe never names the node-selected task-run RuntimeClass' ;;
esac
pass_format_count=$(grep -Fc \
  "printf 'PASS node=%s handler=runc-cgroupwritable cgroup_root=/ writable=true isolated=true worker_denials=true\\n'" \
  "$CONFORMANCE" || true)
expect_eq 'PASS printf format occurrences' 2 "$pass_format_count"
owners=$(grep -rl 'label node' "$NODE_DIR" --include='*.sh' | grep -v "$SCRIPT_DIR" | sort)
expect_eq 'sole label owner under deploy/node' "$CONFORMANCE" "$owners"

# ---------------------------------------------------------------------------
# 11. Non-vacuity: the pre-change script restarts k3s on the very fixture the
#     preflight now refuses. Skipped once the baseline carries the preflight.
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
