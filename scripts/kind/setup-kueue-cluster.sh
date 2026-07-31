#!/usr/bin/env bash
# A DISPOSABLE kind cluster with Kueue installed and ARMED (fbiy-B0).
#
# WHAT THIS IS FOR
# ----------------
# Epic fbiy's live-cluster half asks questions that cannot be answered by a
# rendered fixture: does Kueue mutate only `spec.suspend`, does the `pods`
# nominalQuota actually bound admission, what does force-deleting a Workload
# really do. Every one of those needs an API server with Kueue armed against
# it. This script produces exactly one, and nothing else.
#
# It is the enabling half of `server/crates/djinn-k8s/tests/kueue_cluster_harness.rs`,
# which is where the assertions live.
#
# THIS MUST NEVER ARM PRODUCTION
# ------------------------------
# Arming Kueue on the production VPS is a gated operator act behind
# `deploy/kueue/preflight.sh --mode cutover` and belongs to epic 4c9q's
# runbook. It has not happened. Nothing here is a step toward it.
#
# Four independent guards keep it that way, and all of them run BEFORE any
# cluster, registry or kubeconfig is touched (`check` runs them and stops):
#
#   1. The cluster, its context, its registry and its registry port all carry
#      harness-specific names, so the developer's Tilt cluster (`kind-djinn`,
#      `kind-registry`, port 5001) is not merely "probably not selected" — it
#      is a REFUSED name (exit 3).
#   2. A caller-supplied `--context` that is not the context this script is
#      about to create is refused (exit 3). It is never "used anyway". Note
#      that every context in a Djinn developer's kubeconfig today is an EKS
#      cluster, so "whatever is current" is the single worst default available;
#      this script never reads the current context.
#   3. Every `kubectl` and `helm` invocation below is pinned with
#      `--context` / `--kube-context`. There is no unpinned call.
#   4. `up` refuses to adopt a kind cluster that already exists (exit 6). A
#      cluster this script did not create is one whose contents it cannot
#      vouch for, and tearing it down on failure would then destroy someone
#      else's work.
#
# KUBERNETES FLOOR: 1.30
# ----------------------
# Kueue 0.19.0 requires >= 1.30 and this script refuses anything older
# (exit 7). The 1.29 floor that used to be documented was MEASURED FALSE on
# 2026-07-30 and corrected in #2818. `k8s-openapi` is compiled against `v1_30`
# in `server/crates/djinn-k8s/Cargo.toml`, which is the same floor from the
# client side.
#
# THE WRITABLE-CGROUP NODE — `--cgroup-writable`, OFF BY DEFAULT (fbiy-C1)
# ------------------------------------------------------------------------
# Without the flag this script patches containerd for REGISTRIES ONLY, exactly
# like `scripts/kind/setup-kind.sh:59-106`: no `runc-cgroupwritable` runtime
# handler and no `djinn.io/cgroup-writable=true` node label, both of which
# `deploy/helm/djinn/templates/runtimeclass-cgroup-writable.yaml` needs. That
# default is load-bearing — `fbiy-B0`/`B1`/`B2` clusters must stay byte
# identical to what they were measured against — so the C1 node work is opt-in
# rather than unconditional.
#
# `--cgroup-writable` adds exactly two node-level facts, and NOTHING chart-level:
#
#   * a `runc-cgroupwritable` containerd runtime handler on every node, written
#     into the schema the node's LIVE `/etc/containerd/config.toml` declares
#     (resolved with `deploy/node/k3s/containerd-config-version.sh`, the same
#     detector the managed-k3s conformance uses — never from a version string);
#   * the `djinn.io/cgroup-writable=true` node label the RuntimeClass's
#     `scheduling.nodeSelector` requires.
#
# It still installs NO RuntimeClass. That object comes from the chart, gated by
# `cgroupWritable.runtimeClass.enabled`, so `deploy/kueue/preflight.sh --mode
# cutover` continues to exit 10 ("RuntimeClass djinn-cgroup-writable is absent")
# against a `--values` file that leaves the gate off — which is 6knv's AC4, and
# is unaffected by this flag. A caller that wants the class passes a values file
# that enables it (see `deploy/helm/djinn/tests/fixtures/kueue-governor-values.yaml`);
# a caller that enables the class WITHOUT this flag gets Pods that never
# schedule, which is why the flag verifies the handler is live rather than
# assuming the append worked.
#
# USAGE
#   scripts/kind/setup-kueue-cluster.sh up      # create + install + arm
#   scripts/kind/setup-kueue-cluster.sh down    # delete cluster AND registry
#   scripts/kind/setup-kueue-cluster.sh check   # run the guards only, touch nothing
#
#   --cgroup-writable   install the runc-cgroupwritable containerd handler and
#                       label the nodes (fbiy-C1); off by default
#
# `up` deletes the cluster and the registry on FAILURE (that is AC3 of 6knv);
# on success it leaves them running for the Rust harness and the caller is
# responsible for `down`. Pass `--keep-on-failure` to keep a broken cluster for
# debugging — it is opt-in precisely because the default must not leave
# wreckage behind on a host that ran low on disk today.
#
# EXIT CODES
#   0   ok
#   2   usage / caller error
#   3   refused a context or a reserved name this script must not touch
#   4   not enough free disk
#   5   a required tool is missing
#   6   refused to adopt a pre-existing cluster
#   7   Kubernetes version below the Kueue 0.19 floor
#   1   anything else (install failure, API error, ...)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

KIND="${KIND:-kind}"
KUBECTL_BIN="${KUBECTL:-kubectl}"
HELM_BIN="${HELM:-helm}"
DOCKER="${DOCKER:-docker}"

# Harness-specific everything. None of these may collide with the Tilt cluster.
CLUSTER_NAME="${DJINN_KUEUE_HARNESS_CLUSTER:-djinn-kueue-harness}"
REG_NAME="${DJINN_KUEUE_HARNESS_REGISTRY:-djinn-kueue-harness-registry}"
REG_PORT="${DJINN_KUEUE_HARNESS_REG_PORT:-5051}"
KIND_IMAGE_VERSION="${DJINN_KUEUE_HARNESS_K8S_VERSION:-1.31.0}"
MIN_FREE_GIB="${DJINN_KUEUE_HARNESS_MIN_FREE_GIB:-20}"
INSTALL_TIMEOUT_SECONDS="${DJINN_KUEUE_HARNESS_INSTALL_TIMEOUT_SECONDS:-600}"
VALUES_FILE="${DJINN_KUEUE_HARNESS_VALUES:-$REPO_ROOT/deploy/helm/djinn/tests/fixtures/kueue-cluster-values.yaml}"
REQUESTED_CONTEXT=""
KEEP_ON_FAILURE=false
CGROUP_WRITABLE=false
ACTION=""

# The node-level half of the writable-cgroup contract: the `handler:` and the
# `scheduling.nodeSelector` key of
# `deploy/helm/djinn/templates/runtimeclass-cgroup-writable.yaml`. Both are
# literals here because a shell script cannot read a helm template, so the
# handler is ASSERTED equal to `containerd-config-version.sh`'s own
# `DJINN_CONTAINERD_HANDLER` below — a rename on either side is then a hard
# failure rather than two spellings that never meet.
CGROUP_WRITABLE_HANDLER="runc-cgroupwritable"
CGROUP_WRITABLE_NODE_LABEL="djinn.io/cgroup-writable"
CONTAINERD_LIVE_CONFIG="/etc/containerd/config.toml"
CONTAINERD_VERSION_LIB="$REPO_ROOT/deploy/node/k3s/containerd-config-version.sh"

PREREQS_CHART="$REPO_ROOT/deploy/helm/djinn-prereqs"
PREREQS_RELEASE="djinn-prereqs"
PREREQS_NAMESPACE="kueue-system"
CHART="$REPO_ROOT/deploy/helm/djinn"
RELEASE="djinn"
NAMESPACE="djinn"

# The names this script must never touch, whatever it is asked to do.
# `djinn` / `kind-registry` / 5001 are `scripts/kind/setup-kind.sh`'s defaults,
# i.e. the developer's live Tilt environment.
RESERVED_CLUSTER_NAMES=(djinn kind)
RESERVED_REGISTRY_NAMES=(kind-registry)
RESERVED_REG_PORTS=(5000 5001)

EXIT_USAGE=2
EXIT_REFUSED_TARGET=3
EXIT_LOW_DISK=4
EXIT_MISSING_TOOL=5
EXIT_CLUSTER_EXISTS=6
EXIT_VERSION_FLOOR=7

MIN_K8S_MINOR=30

# The header comment above IS the help text. Printing it rather than
# maintaining a second copy is why the range stops at the last commented line
# (`#   1   anything else ...`), which `awk` finds instead of a line number that
# would silently drift on the next edit.
usage() {
    awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' \
        "${BASH_SOURCE[0]}" >&2
}

fail() {
    local code=$1
    shift
    printf 'FAIL: %s\n' "$*" >&2
    exit "$code"
}

info() { printf '>>> %s\n' "$*"; }

# --- Argument parsing -------------------------------------------------------
while [ "$#" -gt 0 ]; do
    case "$1" in
        up|down|check)
            [ -z "$ACTION" ] || fail "$EXIT_USAGE" "two actions given: $ACTION and $1"
            ACTION=$1
            shift
            ;;
        --context)
            [ "$#" -ge 2 ] || fail "$EXIT_USAGE" '--context requires a value'
            REQUESTED_CONTEXT=$2
            shift 2
            ;;
        --cluster-name)
            [ "$#" -ge 2 ] || fail "$EXIT_USAGE" '--cluster-name requires a value'
            CLUSTER_NAME=$2
            shift 2
            ;;
        --registry-name)
            [ "$#" -ge 2 ] || fail "$EXIT_USAGE" '--registry-name requires a value'
            REG_NAME=$2
            shift 2
            ;;
        --registry-port)
            [ "$#" -ge 2 ] || fail "$EXIT_USAGE" '--registry-port requires a value'
            REG_PORT=$2
            shift 2
            ;;
        --k8s-version)
            [ "$#" -ge 2 ] || fail "$EXIT_USAGE" '--k8s-version requires a value'
            KIND_IMAGE_VERSION=$2
            shift 2
            ;;
        --values)
            [ "$#" -ge 2 ] || fail "$EXIT_USAGE" '--values requires a value'
            VALUES_FILE=$2
            shift 2
            ;;
        --min-free-gib)
            [ "$#" -ge 2 ] || fail "$EXIT_USAGE" '--min-free-gib requires a value'
            MIN_FREE_GIB=$2
            shift 2
            ;;
        --keep-on-failure)
            KEEP_ON_FAILURE=true
            shift
            ;;
        --cgroup-writable)
            CGROUP_WRITABLE=true
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *) fail "$EXIT_USAGE" "unknown option: $1" ;;
    esac
done

ACTION="${ACTION:-up}"

# --- Guards (all of them run before anything is created) --------------------

[[ "$CLUSTER_NAME" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] \
    || fail "$EXIT_USAGE" "--cluster-name must be a DNS label, got: $CLUSTER_NAME"
[[ "$REG_PORT" =~ ^[1-9][0-9]{2,4}$ ]] \
    || fail "$EXIT_USAGE" "--registry-port must be a TCP port, got: $REG_PORT"
[[ "$MIN_FREE_GIB" =~ ^[0-9]+$ ]] \
    || fail "$EXIT_USAGE" "--min-free-gib must be a non-negative integer, got: $MIN_FREE_GIB"

# Guard 1: reserved names. The Tilt cluster is not "unlikely to be selected";
# it is unselectable.
for reserved in "${RESERVED_CLUSTER_NAMES[@]}"; do
    [ "$CLUSTER_NAME" != "$reserved" ] || fail "$EXIT_REFUSED_TARGET" \
        "refusing to operate on kind cluster '$CLUSTER_NAME': that is the developer's Tilt cluster (scripts/kind/setup-kind.sh). This harness creates and DELETES its target, so it must own the name outright."
done
for reserved in "${RESERVED_REGISTRY_NAMES[@]}"; do
    [ "$REG_NAME" != "$reserved" ] || fail "$EXIT_REFUSED_TARGET" \
        "refusing to operate on registry '$REG_NAME': that is the Tilt registry (scripts/kind/setup-kind.sh), and 'down' deletes the registry it is given."
done
for reserved in "${RESERVED_REG_PORTS[@]}"; do
    [ "$REG_PORT" != "$reserved" ] || fail "$EXIT_REFUSED_TARGET" \
        "refusing registry port $REG_PORT: 5001 is the Tilt registry's published port and binding it would either fail or shadow it."
done

# Guard 2: the Kubernetes floor, checked on the REQUESTED image before a
# cluster exists, and re-checked against the live API server after create.
K8S_MAJOR=${KIND_IMAGE_VERSION%%.*}
K8S_REST=${KIND_IMAGE_VERSION#*.}
K8S_MINOR=${K8S_REST%%.*}
[[ "$K8S_MAJOR" =~ ^[0-9]+$ && "$K8S_MINOR" =~ ^[0-9]+$ ]] \
    || fail "$EXIT_USAGE" "--k8s-version must look like 1.31.0, got: $KIND_IMAGE_VERSION"
if [ "$K8S_MAJOR" -lt 1 ] || { [ "$K8S_MAJOR" -eq 1 ] && [ "$K8S_MINOR" -lt "$MIN_K8S_MINOR" ]; }; then
    fail "$EXIT_VERSION_FLOOR" \
        "Kubernetes $KIND_IMAGE_VERSION is below the 1.$MIN_K8S_MINOR floor Kueue 0.19.0 requires. The 1.29 floor this repository used to document was measured false on 2026-07-30 and corrected in #2818; do not restore it."
fi

# Guard 3: the context. Derived, never discovered — this script does not read
# the current context, because on a Djinn developer's machine every context in
# the kubeconfig is an EKS cluster.
CONTEXT="kind-${CLUSTER_NAME}"
if [ -n "$REQUESTED_CONTEXT" ] && [ "$REQUESTED_CONTEXT" != "$CONTEXT" ]; then
    fail "$EXIT_REFUSED_TARGET" \
        "refusing --context '$REQUESTED_CONTEXT': this harness only ever targets '$CONTEXT', the context of the kind cluster it creates and deletes itself. An externally supplied context is a cluster whose contents this script cannot vouch for — and every context in a Djinn developer's kubeconfig today is a live EKS cluster (demo/staging/prod)."
fi

KUBECTL=("$KUBECTL_BIN" --context "$CONTEXT")
HELM=("$HELM_BIN" --kube-context "$CONTEXT")

if [ "$ACTION" = check ]; then
    printf 'PASS: guards accept cluster=%s context=%s registry=%s:%s k8s=%s cgroup-writable=%s\n' \
        "$CLUSTER_NAME" "$CONTEXT" "$REG_NAME" "$REG_PORT" "$KIND_IMAGE_VERSION" "$CGROUP_WRITABLE"
    exit 0
fi

require_tool() {
    command -v "$1" >/dev/null 2>&1 || fail "$EXIT_MISSING_TOOL" "$1 is not on PATH; this harness needs docker, kind, kubectl and helm"
}
require_tool "$DOCKER"
require_tool "$KIND"
require_tool "$KUBECTL_BIN"
require_tool "$HELM_BIN"

# --- Teardown ---------------------------------------------------------------
# Deletes only the cluster and registry NAMES this run validated above. Called
# both by `down` and by `up`'s failure trap.
teardown() {
    info "deleting kind cluster ${CLUSTER_NAME}"
    "$KIND" delete cluster --name "$CLUSTER_NAME" || true
    if "$DOCKER" inspect "$REG_NAME" >/dev/null 2>&1; then
        info "deleting registry container ${REG_NAME}"
        "$DOCKER" rm -f "$REG_NAME" >/dev/null || true
    else
        info "registry container ${REG_NAME} is already absent"
    fi
    # Prove the deletion rather than asserting it: a teardown that reported
    # success while the cluster survived is how a "disposable" harness turns
    # into a permanent one on a host that is already at 85% disk.
    local surviving
    surviving=$("$KIND" get clusters 2>/dev/null | grep -Fx "$CLUSTER_NAME" || true)
    if [ -n "$surviving" ]; then
        printf 'FAIL: kind cluster %s survived teardown\n' "$CLUSTER_NAME" >&2
        return 1
    fi
    if "$DOCKER" inspect "$REG_NAME" >/dev/null 2>&1; then
        printf 'FAIL: registry container %s survived teardown\n' "$REG_NAME" >&2
        return 1
    fi
    printf 'PASS: cluster %s and registry %s are gone\n' "$CLUSTER_NAME" "$REG_NAME"
}

if [ "$ACTION" = down ]; then
    teardown
    exit 0
fi

# --- up ---------------------------------------------------------------------

[ -f "$VALUES_FILE" ] || fail "$EXIT_USAGE" "values file does not exist: $VALUES_FILE"
[ -d "$PREREQS_CHART" ] || fail 1 "Kueue prerequisite chart is missing: $PREREQS_CHART"
[ -f "$PREREQS_CHART/Chart.lock" ] || fail 1 "prerequisite chart is unpinned: $PREREQS_CHART/Chart.lock is missing"
# Same reasoning as deploy/kueue/zero-capture-gate.sh: install the exact
# reviewed bytes, never a resolve-at-install-time dependency.
ls "$PREREQS_CHART"/charts/kueue-*.tgz >/dev/null 2>&1 \
    || fail 1 "prerequisite chart has no vendored kueue dependency; run: helm dependency update $PREREQS_CHART"
[ -d "$CHART" ] || fail 1 "Djinn chart is missing: $CHART"

# Guard 4: never adopt a cluster this run did not create.
if "$KIND" get clusters 2>/dev/null | grep -qx "$CLUSTER_NAME"; then
    fail "$EXIT_CLUSTER_EXISTS" \
        "kind cluster '$CLUSTER_NAME' already exists. This harness refuses to adopt it: it tears its cluster down on failure, and a cluster it did not create is one whose contents it cannot vouch for. Delete it first: $0 down --cluster-name $CLUSTER_NAME"
fi

# Disk. The host ran low on 2026-07-30 and a kind node plus a Kueue install is
# not a small thing to discover you cannot afford halfway through.
#
# An unparseable `df` is a failure, not zero free space and not a skip. Both of
# the wrong answers are worse than stopping: "0Gi free" sends an operator
# pruning a disk that is fine, and skipping the check reintroduces the very
# mid-install disk exhaustion this guard exists for.
FREE_GIB=$(df -BG --output=avail / 2>/dev/null | tail -1 | tr -dc '0-9')
[ -n "$FREE_GIB" ] || fail 1 \
    "could not read free space on / (df -BG --output=avail is GNU coreutils syntax). Fix the check rather than removing it."
info "free space on /: ${FREE_GIB}Gi (floor ${MIN_FREE_GIB}Gi)"
[ "$FREE_GIB" -ge "$MIN_FREE_GIB" ] || fail "$EXIT_LOW_DISK" \
    "only ${FREE_GIB}Gi free on / but this harness wants ${MIN_FREE_GIB}Gi. Prune before retrying (agent worktree cargo targets and cargo-target-runs are the usual culprits); do not lower the floor to get past this."

on_exit() {
    local status=$?
    [ "$status" -eq 0 ] && return 0
    if [ "$KEEP_ON_FAILURE" = true ]; then
        printf 'WARN: harness failed (status %s) and --keep-on-failure was given; cluster %s is STILL RUNNING. Delete it with: %s down --cluster-name %s\n' \
            "$status" "$CLUSTER_NAME" "$0" "$CLUSTER_NAME" >&2
        return 0
    fi
    printf 'INFO: harness failed (status %s); tearing the disposable cluster down\n' "$status" >&2
    teardown || true
}
trap on_exit EXIT

# 1. Registry, on a harness-specific name and port.
if "$DOCKER" inspect "$REG_NAME" >/dev/null 2>&1; then
    fail "$EXIT_CLUSTER_EXISTS" \
        "registry container '$REG_NAME' already exists; this harness does not adopt one. Remove it first: $0 down --cluster-name $CLUSTER_NAME --registry-name $REG_NAME"
fi
info "starting registry ${REG_NAME} at 127.0.0.1:${REG_PORT}"
"$DOCKER" run -d --restart=no \
    -p "127.0.0.1:${REG_PORT}:5000" \
    --network bridge \
    --name "$REG_NAME" \
    registry:2 >/dev/null

# 2. The cluster.
info "creating kind cluster ${CLUSTER_NAME} at Kubernetes v${KIND_IMAGE_VERSION}"
"$KIND" create cluster --name "$CLUSTER_NAME" --image "kindest/node:v${KIND_IMAGE_VERSION}" --config=- <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
containerdConfigPatches:
  - |-
    [plugins."io.containerd.grpc.v1.cri".registry]
      config_path = "/etc/containerd/certs.d"
nodes:
  - role: control-plane
EOF

# `kind create cluster` switches the current context to the new cluster. Every
# call below is still pinned; this is only so a caller left on the harness
# context is left on a context that exists.
info "created context ${CONTEXT} (current-context is now this disposable cluster)"

# Re-check the floor against the LIVE API server. The kind node image tag is a
# claim; `kubectl version` is the measurement. This is the same distinction
# that made the documented 1.29 floor wrong for five minor versions.
SERVER_MINOR=$("${KUBECTL[@]}" version -o json | sed -n 's/.*"minor": *"\([0-9]*\)".*/\1/p' | tail -1)
[[ "$SERVER_MINOR" =~ ^[0-9]+$ ]] || fail 1 "could not read the live Kubernetes minor version from context $CONTEXT"
[ "$SERVER_MINOR" -ge "$MIN_K8S_MINOR" ] || fail "$EXIT_VERSION_FLOOR" \
    "the live API server reports 1.${SERVER_MINOR}, below the 1.${MIN_K8S_MINOR} floor Kueue 0.19.0 requires"
info "live API server is Kubernetes 1.${SERVER_MINOR} (floor 1.${MIN_K8S_MINOR})"

# 3. containerd wiring, per node: registry mirrors always, and — only under
#    `--cgroup-writable` — the `runc-cgroupwritable` runtime handler the
#    RuntimeClass names (fbiy-C1).
#
# WHY THE SCHEMA IS MEASURED AND NOT ASSUMED
# ------------------------------------------
# A kind node's containerd BINARY version and its CRI CONFIGURATION schema are
# different facts, and on this image they disagree: `kindest/node:v1.35.0` ships
# containerd 2.2.0 — which reads schema v3 — under a `/etc/containerd/config.toml`
# whose first non-comment line is literally `version = 2`. The production VPS
# runs the v3 schema (`deploy/node/k3s/containerd/config-v3.toml.tmpl`, pinned at
# containerd 2.2.3-k3s1), so a handler block copied from the repo's production
# asset would land in a plugin namespace this node's config does not use and
# would be silently ignored: the RuntimeClass would resolve, the Pod would be
# admitted, and the sandbox would come up under plain `runc` with a READ-ONLY
# /sys/fs/cgroup. The launcher would then fail readiness for a reason that looks
# nothing like "the handler was written into the wrong table".
#
# So the table header is resolved from the LIVE file by
# `deploy/node/k3s/containerd-config-version.sh` — the same detector the managed
# k3s conformance uses, sourced rather than reimplemented — and the result is
# verified against `crictl info` after the restart. `cgroup_writable` is what
# makes the container's own cgroup writable; `SystemdCgroup` mirrors the node's
# base `runc` handler, because a handler on the other driver places its
# containers under a cgroup parent the kubelet does not manage.
#
# This still installs NO RuntimeClass — that object is the chart's, gated by
# `cgroupWritable.runtimeClass.enabled` — so `deploy/kueue/preflight.sh --mode
# cutover` still exits 10 against a values file that leaves the gate off, which
# is 6knv's AC4.
[ -r "$CONTAINERD_VERSION_LIB" ] || fail 1 \
    "the containerd schema detector is missing: $CONTAINERD_VERSION_LIB. It is the single source of truth for which plugin namespace a node's live configuration uses; do not inline a namespace here to get past this."
# shellcheck source=../../deploy/node/k3s/containerd-config-version.sh
. "$CONTAINERD_VERSION_LIB"

configure_node_containerd() {
    local registry_dir="/etc/containerd/certs.d/localhost:${REG_PORT}"
    local in_cluster_dir="/etc/containerd/certs.d/${REG_NAME}:5000"
    local node
    for node in $("$KIND" get nodes --name "$CLUSTER_NAME"); do
        "$DOCKER" exec "$node" mkdir -p "$registry_dir" "$in_cluster_dir"
        "$DOCKER" exec -i "$node" cp /dev/stdin "$registry_dir/hosts.toml" <<EOF
[host."http://${REG_NAME}:5000"]
  capabilities = ["pull", "resolve"]
EOF
        "$DOCKER" exec -i "$node" cp /dev/stdin "$in_cluster_dir/hosts.toml" <<EOF
[host."http://${REG_NAME}:5000"]
  capabilities = ["pull", "resolve"]
EOF
        [ "$CGROUP_WRITABLE" = true ] || continue
        install_cgroup_writable_handler "$node"
    done
}

# Append the handler to ONE node's live containerd configuration, restart
# containerd, and prove the handler is actually loaded.
install_cgroup_writable_handler() {
    local node=$1 live version table quote namespace
    live=$(mktemp)
    # A `docker exec cat` rather than a bind mount: the file lives inside the
    # node container and the detector reads a path.
    "$DOCKER" exec "$node" cat "$CONTAINERD_LIVE_CONFIG" >"$live" 2>/dev/null \
        || fail 1 "node $node has no readable $CONTAINERD_LIVE_CONFIG; nothing to extend"
    version=$(djinn_containerd_detect_version "$live") \
        || fail 1 "could not resolve the containerd config schema of node $node"
    namespace=$(djinn_containerd_namespace_for_version "$version")
    # `DJINN_CONTAINERD_HANDLER` is the library's own handler name and already
    # defaults to the one the chart names; asserted rather than re-set, so a
    # rename on either side is a hard failure instead of two spellings.
    [ "$DJINN_CONTAINERD_HANDLER" = "$CGROUP_WRITABLE_HANDLER" ] || fail 1 \
        "handler name drift: $CONTAINERD_VERSION_LIB says '$DJINN_CONTAINERD_HANDLER', this script and the chart say '$CGROUP_WRITABLE_HANDLER'"
    table=$(djinn_containerd_runtime_table_for_version "$version")
    rm -f "$live"
    case "$version" in
        2) quote='"' ;;
        3) quote="'" ;;
    esac
    info "node $node runs containerd CRI config schema v${version} (namespace ${namespace}); appending handler ${CGROUP_WRITABLE_HANDLER}"

    # Idempotent: a second `up` against the same node must not double-declare
    # the table, which containerd rejects outright.
    if "$DOCKER" exec "$node" grep -qF "$table" "$CONTAINERD_LIVE_CONFIG"; then
        info "node $node already declares ${table}"
    else
        "$DOCKER" exec -i "$node" tee -a "$CONTAINERD_LIVE_CONFIG" >/dev/null <<EOF

# Added by scripts/kind/setup-kueue-cluster.sh --cgroup-writable (fbiy-C1).
# Mirrors the managed-k3s templates in deploy/node/k3s/containerd/, rendered in
# the schema this node's live configuration declares.
${table}
  runtime_type = "io.containerd.runc.v2"
  cgroup_writable = true
[plugins.${quote}${namespace}${quote}.containerd.runtimes.${CGROUP_WRITABLE_HANDLER}.options]
  SystemdCgroup = true
EOF
    fi

    "$DOCKER" exec "$node" systemctl restart containerd \
        || fail 1 "containerd did not restart on node $node after the handler was appended"

    # Prove it, do not assume it. `crictl info` reports the runtime table
    # containerd actually PARSED; a handler in the wrong namespace, or a config
    # containerd refused, is absent here while the file on disk still shows it.
    local attempt=0 runtimes=""
    while [ "$attempt" -lt 60 ]; do
        runtimes=$("$DOCKER" exec "$node" crictl info 2>/dev/null) || runtimes=""
        case "$runtimes" in
            *"$CGROUP_WRITABLE_HANDLER"*) break ;;
        esac
        attempt=$((attempt + 1))
        sleep 1
    done
    case "$runtimes" in
        *"$CGROUP_WRITABLE_HANDLER"*) ;;
        *) fail 1 "containerd on node $node never reported the ${CGROUP_WRITABLE_HANDLER} handler after restart; the table was written into ${namespace} for schema v${version}" ;;
    esac
    case "$runtimes" in
        *cgroup_writable*|*CgroupWritable*|*cgroupWritable*) ;;
        *) fail 1 "containerd on node $node loaded ${CGROUP_WRITABLE_HANDLER} but reports no cgroup_writable property; this containerd is too old to give a container a writable cgroup, so the RuntimeClass would resolve, the Pod would be admitted, and the launcher would fail readiness on a read-only /sys/fs/cgroup. Use a newer node image (--k8s-version 1.35.0 ships containerd 2.2.0, measured 2026-07-31)" ;;
    esac
    info "node $node: containerd reports the ${CGROUP_WRITABLE_HANDLER} handler with a cgroup_writable property"
}

info 'wiring containerd registry mirrors'
configure_node_containerd

if [ "$CGROUP_WRITABLE" = true ]; then
    # The label half of the contract. `RuntimeClass/djinn-cgroup-writable`
    # carries `scheduling.nodeSelector: djinn.io/cgroup-writable: "true"`, which
    # the RuntimeClass admission controller merges into every Pod naming the
    # class — so without this an armed task-run Pod is permanently Pending with
    # no event that mentions cgroups at all.
    for node in $("$KIND" get nodes --name "$CLUSTER_NAME"); do
        info "labelling node ${node} ${CGROUP_WRITABLE_NODE_LABEL}=true"
        "${KUBECTL[@]}" label node "$node" "${CGROUP_WRITABLE_NODE_LABEL}=true" --overwrite
    done
    # The API server and the kubelet both went through a containerd restart;
    # wait for the node to be Ready again before helm starts creating objects.
    "${KUBECTL[@]}" wait --for=condition=Ready node --all --timeout=180s
fi

if [ "$("$DOCKER" inspect -f '{{json .NetworkSettings.Networks.kind}}' "$REG_NAME" 2>/dev/null)" = 'null' ]; then
    "$DOCKER" network connect kind "$REG_NAME"
fi

# 4. The pinned Kueue prerequisite. `--wait` is mandatory here, not optional:
# ClusterQueue and LocalQueue carry a conversion webhook, so the djinn chart's
# topology cannot be applied until the Kueue controller is actually serving.
info "installing pinned Kueue prerequisite ${PREREQS_RELEASE} (Kueue 0.19.0, DRA gates off)"
"${HELM[@]}" upgrade --install "$PREREQS_RELEASE" "$PREREQS_CHART" \
    --namespace "$PREREQS_NAMESPACE" --create-namespace \
    --wait --timeout "${INSTALL_TIMEOUT_SECONDS}s"

# 5. The djinn chart, ARMED.
#
# Deliberately NOT `--wait`. The harness proves an admission contract, not a
# running Djinn: the server image tag in the values file resolves to nothing
# here and its Deployment is expected to sit in ImagePullBackOff forever.
# Waiting would time out on a state that is by design. Every object the
# assertions read — the Namespace label, the ClusterQueue, the three
# LocalQueues — is created synchronously by this call and is present the moment
# it returns.
info "installing djinn chart release ${RELEASE} with kueue.enabled=true kueue.armed=true"
"${HELM[@]}" upgrade --install "$RELEASE" "$CHART" \
    --namespace "$NAMESPACE" --create-namespace \
    --values "$VALUES_FILE" \
    --set kueue.enabled=true --set kueue.armed=true

# 6. Report what the API server actually holds, so a human reading the log sees
# the same three facts the Rust harness asserts. These are printed, not
# asserted: the assertions live in kueue_cluster_harness.rs, and duplicating
# them here in a weaker form would invite someone to trust the weaker copy.
info 'armed cluster state:'
"${KUBECTL[@]}" get namespace "$NAMESPACE" \
    -o 'jsonpath={.metadata.name}{" djinn.io/kueue-managed="}{.metadata.labels.djinn\.io/kueue-managed}{"\n"}'
"${KUBECTL[@]}" get clusterqueues.kueue.x-k8s.io \
    -o 'jsonpath={range .items[*]}{"clusterqueue "}{.metadata.name}{" pods="}{.spec.resourceGroups[0].flavors[0].resources[0].nominalQuota}{"\n"}{end}'
"${KUBECTL[@]}" get localqueues.kueue.x-k8s.io -n "$NAMESPACE" \
    -o 'jsonpath={range .items[*]}{"localqueue "}{.metadata.name}{"\n"}{end}'

cat <<EOF

>>> disposable armed-Kueue cluster '${CLUSTER_NAME}' is ready.

    context:   ${CONTEXT}
    registry:  ${REG_NAME} (127.0.0.1:${REG_PORT})

Run the harness:

    DJINN_TEST_KUEUE_CLUSTER=1 cargo test -p djinn-k8s --test kueue_cluster_harness -- --ignored

TEAR IT DOWN when you are finished — cluster AND registry:

    $0 down --cluster-name ${CLUSTER_NAME} --registry-name ${REG_NAME}
EOF
