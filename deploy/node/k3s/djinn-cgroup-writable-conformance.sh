#!/usr/bin/env bash
# Conform one managed k3s node before it is eligible for the writable-cgroup
# RuntimeClass. This program is deliberately the sole deployment owner of
# djinn.io/cgroup-writable: it removes the label before every risky operation
# and adds it only after all observations have passed.
set -Eeuo pipefail

LABEL='djinn.io/cgroup-writable'
HANDLER='runc-cgroupwritable'
RUNTIME_CLASS='djinn-cgroup-writable'
# Resolved from the live containerd configuration's own `version` key before
# anything is written. There is no default: a node whose generation cannot be
# resolved gets no template and no restart.
RUNTIME_TABLE=''
CONFIG_VERSION=''
TEMPLATE_BASENAME=''
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
VERSION_LIB=${DJINN_CGROUP_VERSION_LIB:-"$SCRIPT_DIR/containerd-config-version.sh"}
TEMPLATE_DIR=${DJINN_CGROUP_TEMPLATE_DIR:-"$SCRIPT_DIR/containerd"}
INSTALL_DIR=${DJINN_CGROUP_INSTALL_DIR:-/var/lib/rancher/k3s/agent/etc/containerd}
# Both paths default to the version-resolved basename inside those directories.
TEMPLATE_SOURCE=${DJINN_CGROUP_TEMPLATE_SOURCE:-}
TEMPLATE_PATH=${DJINN_CGROUP_TEMPLATE_PATH:-}
LIVE_CONFIG_PATH=${DJINN_CGROUP_LIVE_CONFIG_PATH:-/var/lib/rancher/k3s/agent/etc/containerd/config.toml}
KUBECTL=${DJINN_KUBECTL:-kubectl}
K3S_RESTART_CMD=${DJINN_K3S_RESTART_CMD:-'systemctl restart k3s'}
PROBE_IMAGE=${DJINN_CGROUP_PROBE_IMAGE:-ubuntu@sha256:4fbb8e6a8395de5a7550b33509421a2bafbc0aab6c06ba2cef9ebffbc7092d90}
TIMEOUT=${DJINN_CGROUP_TIMEOUT:-120s}
FIXTURE_MODE=${DJINN_CGROUP_FIXTURE_MODE:-}
FIXTURE_CASE=${DJINN_CGROUP_FIXTURE_CASE:-success}
FIXTURE_LOG=${DJINN_CGROUP_FIXTURE_LOG:-}
NODE_NAME=''
PROBE_NAME=''
SUCCESS=0
TEMPLATE_INSTALLED=0
TEMPLATE_BACKUP=''
BACKUP_DIR=''

usage() {
  cat >&2 <<'USAGE'
usage: djinn-cgroup-writable-conformance.sh --node NODE

Environment seams (for hermetic fixture tests):
  DJINN_CGROUP_FIXTURE_MODE=1
  DJINN_CGROUP_FIXTURE_CASE=success|timeout|wrong-node|sandbox|isolation|readonly|mutation-success|handler-removed|version-mismatch
  DJINN_CGROUP_FIXTURE_LOG=PATH

Production seams: DJINN_KUBECTL, DJINN_K3S_RESTART_CMD,
DJINN_CGROUP_TEMPLATE_SOURCE, DJINN_CGROUP_TEMPLATE_PATH,
DJINN_CGROUP_TEMPLATE_DIR, DJINN_CGROUP_INSTALL_DIR,
DJINN_CGROUP_VERSION_LIB, DJINN_CGROUP_LIVE_CONFIG_PATH,
DJINN_CGROUP_PROBE_IMAGE, and DJINN_CGROUP_TIMEOUT.

The containerd CRI configuration generation is resolved from the live
configuration's own top-level `version` key, never from a k3s version string.
Version 2 (or an absent key) selects config.toml.tmpl and the
io.containerd.grpc.v1.cri namespace; version 3 selects config-v3.toml.tmpl and
the io.containerd.cri.v1.runtime namespace; any other value aborts before the
template is written and before k3s is restarted.
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
      printf 'PASS node=%s handler=runc-cgroupwritable cgroup_root=/ writable=true isolated=true worker_denials=true\n' "$NODE_NAME"
      SUCCESS=1
      ;;
    timeout|wrong-node|sandbox|isolation|readonly|mutation-success|handler-removed|version-mismatch)
      fixture_log "failure=$FIXTURE_CASE"
      fixture_log "cleanup node=$NODE_NAME"
      die "fixture $FIXTURE_CASE"
      ;;
    *) die "unknown fixture case: $FIXTURE_CASE" ;;
  esac
}

# A node must never be left running an unproven base template. Whenever the
# template was installed and conformance did not succeed, put the previous
# bytes back (or remove the file when there were none) and restart k3s again so
# containerd re-renders from the restored source.
restore_template() {
  [[ $TEMPLATE_INSTALLED -eq 1 ]] || return 0
  TEMPLATE_INSTALLED=0
  if [[ -n "$TEMPLATE_BACKUP" ]]; then
    cp -p "$TEMPLATE_BACKUP" "$TEMPLATE_PATH" || return 0
  else
    rm -f "$TEMPLATE_PATH" || return 0
  fi
  eval "$K3S_RESTART_CMD" >/dev/null 2>&1 || true
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
  if [[ $SUCCESS -ne 1 ]]; then
    restore_template || true
  fi
  if [[ -n "$BACKUP_DIR" ]]; then
    rm -rf "$BACKUP_DIR" || true
  fi
  exit "$status"
}
trap cleanup EXIT

require_command() { command -v "$1" >/dev/null 2>&1 || die "required command missing: $1"; }

extract_runtime_table() {
  awk -v header="$RUNTIME_TABLE" '
    $0 == header { in_table=1 }
    in_table {
      if (seen && /^\[/) exit
      seen=1
      sub(/[[:space:]]*#.*/, "")
      if ($0 !~ /^[[:space:]]*$/) print
    }
  ' "$1"
}

expected_runtime_table() {
  printf '%s\n  runtime_type = "io.containerd.runc.v2"\n  cgroup_writable = true' "$RUNTIME_TABLE"
}

# Resolve the containerd configuration generation and prove that the template
# about to be installed, the destination filename and the table this program
# will later validate all describe that same generation. Every step here is a
# pure read: nothing is written and k3s is not restarted, so a node whose
# generation has no matching asset in this repository is left exactly as found.
preflight_version_tuple() {
  local other_version other_table source_table destination_basename source_basename
  CONFIG_VERSION=$(djinn_containerd_detect_version "$LIVE_CONFIG_PATH") ||
    die "cannot resolve the containerd config version from $LIVE_CONFIG_PATH"
  TEMPLATE_BASENAME=$(djinn_containerd_template_basename_for_version "$CONFIG_VERSION") ||
    die "no managed template for containerd config version $CONFIG_VERSION"
  RUNTIME_TABLE=$(djinn_containerd_runtime_table_for_version "$CONFIG_VERSION") ||
    die "no runtime table for containerd config version $CONFIG_VERSION"
  : "${TEMPLATE_SOURCE:=$TEMPLATE_DIR/$TEMPLATE_BASENAME}"
  : "${TEMPLATE_PATH:=$INSTALL_DIR/$TEMPLATE_BASENAME}"
  [[ -r "$TEMPLATE_SOURCE" ]] || die "managed template source is not readable: $TEMPLATE_SOURCE"

  # k3s reads one template filename per configuration generation. A path whose
  # basename asserts a generation must assert the detected one; a basename that
  # asserts nothing (an operator or test seam) is not a contradiction.
  destination_basename=${TEMPLATE_PATH##*/}
  source_basename=${TEMPLATE_SOURCE##*/}
  case "$destination_basename" in
    config.toml.tmpl | config-v3.toml.tmpl)
      [[ "$destination_basename" == "$TEMPLATE_BASENAME" ]] ||
        die "destination $destination_basename contradicts containerd config version $CONFIG_VERSION (expected $TEMPLATE_BASENAME)" ;;
  esac
  case "$source_basename" in
    config.toml.tmpl | config-v3.toml.tmpl)
      [[ "$source_basename" == "$TEMPLATE_BASENAME" ]] ||
        die "template source $source_basename contradicts containerd config version $CONFIG_VERSION (expected $TEMPLATE_BASENAME)" ;;
  esac

  # The template's own text is the claim that matters: a v2 template on a v3
  # node renders a namespace containerd 2.x ignores entirely, which is exactly
  # how an armed launcher reaches a node that cannot run the handler.
  if [[ "$CONFIG_VERSION" == 2 ]]; then other_version=3; else other_version=2; fi
  other_table=$(djinn_containerd_runtime_table_for_version "$other_version")
  ! grep -Fq "$other_table" "$TEMPLATE_SOURCE" ||
    die "template source $TEMPLATE_SOURCE declares the version-$other_version runtime table on a version-$CONFIG_VERSION node"
  source_table=$(extract_runtime_table "$TEMPLATE_SOURCE")
  [[ "$source_table" == "$(expected_runtime_table)" ]] ||
    die "template source $TEMPLATE_SOURCE does not contain the exact $HANDLER runtime table for containerd config version $CONFIG_VERSION"
}

validate_live_runtime_table() {
  [[ -r "$LIVE_CONFIG_PATH" ]] || die "live containerd config is not readable: $LIVE_CONFIG_PATH"
  local actual expected
  actual=$(extract_runtime_table "$LIVE_CONFIG_PATH")
  expected=$(expected_runtime_table)
  [[ "$actual" == "$expected" ]] || die "live containerd config does not contain the exact $HANDLER runtime table"
}

install_template() {
  BACKUP_DIR=$(mktemp -d)
  if [[ -e "$TEMPLATE_PATH" ]]; then
    TEMPLATE_BACKUP="$BACKUP_DIR/previous"
    cp -p "$TEMPLATE_PATH" "$TEMPLATE_BACKUP"
  fi
  # Armed before the write so a partial install is still restored.
  TEMPLATE_INSTALLED=1
  install -D -m 0644 "$TEMPLATE_SOURCE" "$TEMPLATE_PATH"
}

ensure_unlabeled() {
  "$KUBECTL" label node "$NODE_NAME" "$LABEL-" --overwrite >/dev/null
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
  runtimeClassName: $RUNTIME_CLASS
  nodeName: $NODE_NAME
  securityContext:
    fsGroup: 1000
    fsGroupChangePolicy: OnRootMismatch
  containers:
  - name: conformance
    image: $PROBE_IMAGE
    command: ["/bin/sh", "-ceu", "while :; do sleep 3600; done"]
    securityContext:
      runAsUser: 0
      allowPrivilegeEscalation: false
      capabilities:
        drop: ["ALL"]
        # Used only for the one-way launcher-to-worker identity transition.
        # The worker verifies that none survive. SETPCAP is needed to empty
        # the capability bounding set before the worker exec.
        add: ["SETUID", "SETGID", "SETPCAP"]
      seccompProfile:
        type: RuntimeDefault
      appArmorProfile:
        type: RuntimeDefault
EOF
}

wait_for_probe() {
  if ! "$KUBECTL" wait --for=condition=Ready "pod/$PROBE_NAME" --timeout="$TIMEOUT" >/dev/null; then
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

# Runs as the root launcher phase of the sole probe process. It proves that the
# runtime gave this container a private cgroup-v2 root; no hostPath or host
# fallback is involved. The same process then permanently becomes the worker.
LAUNCHER_PROBE='set -eu
root=/sys/fs/cgroup
[ "$(stat -fc %T "$root")" = cgroup2fs ]
[ "$(awk -F: '\''$1 == "0" && $2 == "" { print $3 }'\'' /proc/self/cgroup)" = / ]
[ ! -d "$root/system.slice" ]
# A private root has no parent or sibling pod directories visible.
[ -z "$(find "$root" -mindepth 1 -maxdepth 1 -type d -print -quit)" ]
child="$root/.djinn-conformance-child"
# A successful creation alone is insufficient: prove launcher removal too.
mkdir "$child"
rmdir "$child"
[ ! -e "$child" ]
# Retain a launcher-owned leaf so worker denials exercise an existing object.
launcher_leaf="$root/.djinn-launcher-leaf"
mkdir "$launcher_leaf"
[ -d "$launcher_leaf" ]'

# Runs after setpriv irreversibly enters the worker identity in the launcher
# process and its private delegated cgroup namespace. Verify the effective
# security state before making any authorization observation. Every write-like
# operation must fail; reading/traversal remains deliberately permitted.
WORKER_PROBE='set -eu
root=/sys/fs/cgroup
[ "$(awk '\''/^Uid:/ { print $2, $3, $4, $5 }'\'' /proc/self/status)" = "1000 1000 1000 1000" ]
[ "$(awk '\''/^Gid:/ { print $2, $3, $4, $5 }'\'' /proc/self/status)" = "1000 1000 1000 1000" ]
[ "$(awk '\''/^Groups:/ { $1=""; sub(/^[[:space:]]+/, ""); sub(/[[:space:]]+$/, ""); print }'\'' /proc/self/status)" = 1000 ]
[ "$(awk '\''/^NoNewPrivs:/ { print $2 }'\'' /proc/self/status)" = 1 ]
[ "$(awk '\''/^Seccomp:/ { print $2 }'\'' /proc/self/status)" = 2 ]
for capability_set in CapBnd CapEff CapPrm; do
  [ "$(awk -v field="$capability_set:" '\''$1 == field { print $2 }'\'' /proc/self/status)" = 0000000000000000 ]
done
apparmor=$(cat /proc/self/attr/current)
[ -n "$apparmor" ]
[ "$apparmor" != unconfined ]
[ "$(stat -fc %T "$root")" = cgroup2fs ]
[ "$(awk -F: '\''$1 == "0" && $2 == "" { print $3 }'\'' /proc/self/cgroup)" = / ]
ls "$root" >/dev/null
cat "$root/cgroup.controllers" >/dev/null
must_deny() { if sh -c "$1"; then echo "unexpected worker mutation: $1" >&2; exit 1; fi; }
leaf="$root/.djinn-worker-leaf"
launcher_leaf="$root/.djinn-launcher-leaf"
[ -d "$launcher_leaf" ]
ls "$launcher_leaf" >/dev/null
cat "$launcher_leaf/cgroup.controllers" >/dev/null
must_deny "mkdir \"$leaf\""
must_deny "mkdir \"$launcher_leaf/.djinn-worker-child\""
root_cpu_max=$(cat "$root/cpu.max")
[ -n "$root_cpu_max" ]
must_deny "printf '\''%s\\n'\'' \"$root_cpu_max\" > \"$root/cpu.max\""
# Preserve $$ for sh -c so the writer tries to move its own process.
must_deny "printf \"\$\$\" > \"$root/cgroup.procs\""
must_deny "printf 1 > \"$root/cgroup.procs\""
must_deny "printf 1 > \"$root/cgroup.kill\""
launcher_cpu_max=$(cat "$launcher_leaf/cpu.max")
[ -n "$launcher_cpu_max" ]
must_deny "printf '\''%s\\n'\'' \"$launcher_cpu_max\" > \"$launcher_leaf/cpu.max\""
must_deny "printf \"\$\$\" > \"$launcher_leaf/cgroup.procs\""
must_deny "printf 1 > \"$launcher_leaf/cgroup.kill\""
must_deny "rmdir \"$launcher_leaf\""
must_deny "printf 1 > \"$launcher_leaf/cgroup.procs\""'

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
[[ -r "$VERSION_LIB" ]] || die "containerd config version library is not readable: $VERSION_LIB"
# shellcheck source=containerd-config-version.sh
. "$VERSION_LIB"

# Resolve and cross-check the whole version tuple first. It reads only; an
# inconsistent node fails here with no template written and no k3s restart.
preflight_version_tuple

# Remove eligibility before touching either the template or k3s. The EXIT trap
# repeats this on all unsuccessful paths, including restarts and timeouts.
ensure_unlabeled
install_template
eval "$K3S_RESTART_CMD" >/dev/null
validate_live_runtime_table

PROBE_NAME="djinn-cgroup-$(date +%s)-$RANDOM"
PROBE_NAME=${PROBE_NAME:0:63}
render_manifest | "$KUBECTL" apply -f - >/dev/null
wait_for_probe
verify_node_identity
# A single exec process performs both phases so the retained launcher leaf and
# all worker checks necessarily use one private cgroup namespace/root. fsGroup
# supplies supplementary group 1000; setpriv preserves that sole group, changes
# uid/gid, enables no-new-privileges, clears every capability set, and execs the
# worker checks without any path back to launcher authority.
COMBINED_PROBE="$LAUNCHER_PROBE
exec setpriv --reuid=1000 --regid=1000 --keep-groups --nnp --inh-caps=-all --ambient-caps=-all --bounding-set=-all /bin/sh -ceu \"\$1\""
"$KUBECTL" exec "$PROBE_NAME" -c conformance -- /bin/sh -ceu "$COMBINED_PROBE" probe "$WORKER_PROBE" >/dev/null || die 'launcher/worker cgroup conformance failed'

"$KUBECTL" label node "$NODE_NAME" "$LABEL=true" --overwrite >/dev/null
printf 'PASS node=%s handler=runc-cgroupwritable cgroup_root=/ writable=true isolated=true worker_denials=true\n' "$NODE_NAME"
SUCCESS=1
