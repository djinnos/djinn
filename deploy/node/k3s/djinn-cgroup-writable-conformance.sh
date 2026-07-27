#!/usr/bin/env bash
# Conform one managed k3s node before it is eligible for the writable-cgroup
# RuntimeClass. This program is deliberately the sole deployment owner of
# djinn.io/cgroup-writable: it removes the label before every risky operation
# and adds it only after all observations have passed.
set -Eeuo pipefail

LABEL='djinn.io/cgroup-writable'
HANDLER='runc-cgroupwritable'
RUNTIME_TABLE='[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runc-cgroupwritable]'
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
TEMPLATE_SOURCE=${DJINN_CGROUP_TEMPLATE_SOURCE:-"$SCRIPT_DIR/containerd/config.toml.tmpl"}
TEMPLATE_PATH=${DJINN_CGROUP_TEMPLATE_PATH:-/var/lib/rancher/k3s/agent/etc/containerd/config.toml.tmpl}
LIVE_CONFIG_PATH=${DJINN_CGROUP_LIVE_CONFIG_PATH:-/var/lib/rancher/k3s/agent/etc/containerd/config.toml}
KUBECTL=${DJINN_KUBECTL:-kubectl}
K3S_RESTART_CMD=${DJINN_K3S_RESTART_CMD:-'systemctl restart k3s'}
PROBE_IMAGE=${DJINN_CGROUP_PROBE_IMAGE:-busybox:1.36.1}
TIMEOUT=${DJINN_CGROUP_TIMEOUT:-120s}
FIXTURE_MODE=${DJINN_CGROUP_FIXTURE_MODE:-}
FIXTURE_CASE=${DJINN_CGROUP_FIXTURE_CASE:-success}
FIXTURE_LOG=${DJINN_CGROUP_FIXTURE_LOG:-}
NODE_NAME=''
PROBE_NAME=''
SUCCESS=0

usage() {
  cat >&2 <<'USAGE'
usage: djinn-cgroup-writable-conformance.sh --node NODE

Environment seams (for hermetic fixture tests):
  DJINN_CGROUP_FIXTURE_MODE=1
  DJINN_CGROUP_FIXTURE_CASE=success|timeout|wrong-node|sandbox|isolation|readonly|mutation-success|handler-removed
  DJINN_CGROUP_FIXTURE_LOG=PATH

Production seams: DJINN_KUBECTL, DJINN_K3S_RESTART_CMD,
DJINN_CGROUP_TEMPLATE_SOURCE, DJINN_CGROUP_TEMPLATE_PATH,
DJINN_CGROUP_LIVE_CONFIG_PATH, DJINN_CGROUP_PROBE_IMAGE, and
DJINN_CGROUP_TIMEOUT.
USAGE
}

die() { printf 'FAIL %s\n' "$*" >&2; exit 1; }

fixture_log() {
  if [[ -n "$FIXTURE_LOG" ]]; then
    printf '%s\n' "$*" >>"$FIXTURE_LOG"
  fi
}

# The fixture path models label lifecycle only; it never asserts that an
# unprivileged test runner is a k3s node. Real-node execution follows below.
run_fixture() {
  fixture_log "unlabel node=$NODE_NAME"
  case "$FIXTURE_CASE" in
    success)
      fixture_log "template=validated"
      fixture_log "probe=direct-bound node=$NODE_NAME"
      fixture_log 'probe=cgroup-root writable=true isolated=true worker_denials=true'
      fixture_log "label node=$NODE_NAME"
      SUCCESS=1
      printf 'PASS node=%s handler=runc-cgroupwritable cgroup_root=/ writable=true isolated=true worker_denials=true\n' "$NODE_NAME"
      ;;
    timeout|wrong-node|sandbox|isolation|readonly|mutation-success|handler-removed)
      fixture_log "failure=$FIXTURE_CASE"
      fixture_log "cleanup node=$NODE_NAME"
      die "fixture $FIXTURE_CASE"
      ;;
    *) die "unknown fixture case: $FIXTURE_CASE" ;;
  esac
}

cleanup() {
  local status=$?
  # A probe is always short lived. Do not let an API cleanup error mask the
  # conformance result, but do remove eligibility whenever success was not set.
  if [[ -n "$PROBE_NAME" ]]; then
    "$KUBECTL" delete pod "$PROBE_NAME" --ignore-not-found --wait=false >/dev/null 2>&1 || true
  fi
  if [[ $SUCCESS -ne 1 && -n "$NODE_NAME" && -z "$FIXTURE_MODE" ]]; then
    "$KUBECTL" label node "$NODE_NAME" "$LABEL-" --overwrite >/dev/null 2>&1 || true
  fi
  exit "$status"
}
trap cleanup EXIT

require_command() { command -v "$1" >/dev/null 2>&1 || die "required command missing: $1"; }

validate_live_runtime_table() {
  [[ -r "$LIVE_CONFIG_PATH" ]] || die "live containerd config is not readable: $LIVE_CONFIG_PATH"
  local actual expected
  actual=$(awk -v header="$RUNTIME_TABLE" '
    $0 == header { in_table=1 }
    in_table {
      if (seen && /^\[/) exit
      seen=1
      sub(/[[:space:]]*#.*/, "")
      if ($0 !~ /^[[:space:]]*$/) print
    }
  ' "$LIVE_CONFIG_PATH")
  expected=$(printf '%s\n  runtime_type = "io.containerd.runc.v2"\n  cgroup_writable = true' "$RUNTIME_TABLE")
  [[ "$actual" == "$expected" ]] || die "live containerd config does not contain the exact $HANDLER runtime table"
}

ensure_unlabeled() {
  "$KUBECTL" label node "$NODE_NAME" "$LABEL-" --overwrite
  local current
  current=$("$KUBECTL" get node "$NODE_NAME" -o "jsonpath={.metadata.labels.djinn\\.io/cgroup-writable}")
  [[ -z "$current" ]] || die "node remains labeled after removal"
}

render_manifest() {
  cat <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $PROBE_NAME
  labels:
    app.kubernetes.io/name: djinn-cgroup-writable-conformance
spec:
  restartPolicy: Never
  runtimeClassName: $HANDLER
  nodeName: $NODE_NAME
  securityContext:
    fsGroup: 1000
    fsGroupChangePolicy: OnRootMismatch
  containers:
  - name: launcher-context
    image: $PROBE_IMAGE
    command: ["/bin/sh", "-ceu", "while :; do sleep 3600; done"]
    securityContext:
      runAsUser: 0
      allowPrivilegeEscalation: false
      capabilities:
        drop: ["ALL"]
      seccompProfile:
        type: RuntimeDefault
  - name: worker
    image: $PROBE_IMAGE
    command: ["/bin/sh", "-ceu", "while :; do sleep 3600; done"]
    securityContext:
      runAsUser: 1000
      runAsGroup: 1000
      runAsNonRoot: true
      allowPrivilegeEscalation: false
      capabilities:
        drop: ["ALL"]
      seccompProfile:
        type: RuntimeDefault
EOF
}

wait_for_probe() {
  if ! "$KUBECTL" wait --for=condition=Ready "pod/$PROBE_NAME" --timeout="$TIMEOUT"; then
    local pod_json
    pod_json=$("$KUBECTL" get pod "$PROBE_NAME" -o json 2>/dev/null || true)
    if grep -Eq 'FailedCreatePodSandBox|runc-cgroupwritable' <<<"$pod_json"; then
      die "probe sandbox failed for handler $HANDLER"
    fi
    die "probe did not become Ready before $TIMEOUT"
  fi
}

verify_node_identity() {
  local identity
  identity=$("$KUBECTL" get pod "$PROBE_NAME" -o 'jsonpath={.spec.nodeName}|{.status.nodeName}')
  [[ "$identity" == "$NODE_NAME|$NODE_NAME" ]] || die "probe node identity mismatch: $identity"
}

# Runs in the root launcher-context container. It proves that the runtime gave
# this pod a private cgroup-v2 root; no hostPath or host fallback is involved.
LAUNCHER_PROBE='set -eu
root=/sys/fs/cgroup
[ "$(stat -fc %T "$root")" = cgroup2fs ]
[ "$(awk -F: '\''$1 == "0" && $2 == "" { print $3 }'\'' /proc/self/cgroup)" = / ]
[ ! -d "$root/system.slice" ]
# A private root has no parent or sibling pod directories visible.
[ -z "$(find "$root" -mindepth 1 -maxdepth 1 -type d -print -quit)" ]
child="$root/.djinn-conformance-child"
rmdir "$child" 2>/dev/null || true
mkdir "$child"
rmdir "$child"'

# Runs with the exact worker_security_context/pod_security_context contract:
# uid/gid 1000, fsGroup 1000, no_new_privs, ALL capabilities dropped,
# RuntimeDefault seccomp, and the default (not Unconfined) AppArmor profile.
# Every write-like operation must fail; reading/traversal remains deliberately
# permitted and is checked first.
WORKER_PROBE='set -eu
root=/sys/fs/cgroup
[ "$(stat -fc %T "$root")" = cgroup2fs ]
[ "$(awk -F: '\''$1 == "0" && $2 == "" { print $3 }'\'' /proc/self/cgroup)" = / ]
ls "$root" >/dev/null
cat "$root/cgroup.controllers" >/dev/null
must_deny() { if sh -c "$1"; then echo "unexpected worker mutation: $1" >&2; exit 1; fi; }
leaf="$root/.djinn-worker-leaf"
launcher_leaf="$root/.djinn-launcher-leaf"
must_deny "mkdir \"$leaf\""
must_deny "rmdir \"$leaf\""
must_deny "printf x > \"$root/cpu.max\""
must_deny "printf \"$$\" > \"$root/cgroup.procs\""
must_deny "printf 1 > \"$root/cgroup.kill\""
must_deny "mkdir \"$launcher_leaf\""
must_deny "rmdir \"$launcher_leaf\""
must_deny "printf x > \"$launcher_leaf/cpu.max\""
must_deny "printf \"$$\" > \"$launcher_leaf/cgroup.procs\""
must_deny "printf \"1\" > \"$root/cgroup.procs\""'

while [[ $# -gt 0 ]]; do
  case "$1" in
    --node) [[ $# -ge 2 ]] || die '--node requires a value'; NODE_NAME=$2; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) usage; die "unknown argument: $1" ;;
  esac
done
[[ -n "$NODE_NAME" ]] || { usage; die '--node is required'; }

if [[ -n "$FIXTURE_MODE" ]]; then
  run_fixture
  exit 0
fi

[[ $(id -u) -eq 0 ]] || die 'must run as root on the managed k3s node'
require_command "$KUBECTL"
require_command install
require_command awk
require_command systemctl
[[ -r "$TEMPLATE_SOURCE" ]] || die "managed template source is not readable: $TEMPLATE_SOURCE"

# Remove eligibility before touching either the template or k3s. The EXIT trap
# repeats this on all unsuccessful paths, including restarts and timeouts.
ensure_unlabeled
install -D -m 0644 "$TEMPLATE_SOURCE" "$TEMPLATE_PATH"
eval "$K3S_RESTART_CMD"
validate_live_runtime_table

PROBE_NAME="djinn-cgroup-$(date +%s)-$RANDOM"
PROBE_NAME=${PROBE_NAME:0:63}
render_manifest | "$KUBECTL" apply -f -
wait_for_probe
verify_node_identity
"$KUBECTL" exec "$PROBE_NAME" -c launcher-context -- /bin/sh -ceu "$LAUNCHER_PROBE" || die 'launcher cgroup root is not writable and isolated'
"$KUBECTL" exec "$PROBE_NAME" -c worker -- /bin/sh -ceu "$WORKER_PROBE" || die 'worker security context permitted a cgroup mutation'

"$KUBECTL" label node "$NODE_NAME" "$LABEL=true" --overwrite
SUCCESS=1
printf 'PASS node=%s handler=runc-cgroupwritable cgroup_root=/ writable=true isolated=true worker_denials=true\n' "$NODE_NAME"
