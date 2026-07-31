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
# WHAT IT DOES NOT DO — the fbiy-C1 hook
# --------------------------------------
# It patches containerd for REGISTRIES ONLY, exactly like
# `scripts/kind/setup-kind.sh:59-106`. It installs NO `runc-cgroupwritable`
# runtime handler and applies NO `djinn.io/cgroup-writable=true` node label,
# both of which `deploy/helm/djinn/templates/runtimeclass-cgroup-writable.yaml`
# needs. That is deliberate and it is load-bearing twice over:
#
#   * `deploy/kueue/preflight.sh --mode cutover` must exit 10 ("RuntimeClass
#     djinn-cgroup-writable is absent") against this cluster. Installing the
#     RuntimeClass here would silently retire that assertion.
#   * fbiy-C1 owns proving the governor end to end and will need a real
#     writable-cgroup node. See `configure_node_containerd` below: the C1
#     extension goes inside that loop, plus a node label after cluster create.
#     Nothing else in this script has to move.
#
# USAGE
#   scripts/kind/setup-kueue-cluster.sh up      # create + install + arm
#   scripts/kind/setup-kueue-cluster.sh down    # delete cluster AND registry
#   scripts/kind/setup-kueue-cluster.sh check   # run the guards only, touch nothing
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
ACTION=""

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
    printf 'PASS: guards accept cluster=%s context=%s registry=%s:%s k8s=%s\n' \
        "$CLUSTER_NAME" "$CONTEXT" "$REG_NAME" "$REG_PORT" "$KIND_IMAGE_VERSION"
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
FREE_GIB=$(df -BG --output=avail / | tail -1 | tr -dc '0-9')
info "free space on /: ${FREE_GIB}Gi (floor ${MIN_FREE_GIB}Gi)"
[ "${FREE_GIB:-0}" -ge "$MIN_FREE_GIB" ] || fail "$EXIT_LOW_DISK" \
    "only ${FREE_GIB}Gi free on / but this harness wants ${MIN_FREE_GIB}Gi. Prune before retrying (agent worktree cargo targets and cargo-target-runs are the usual culprits); do not lower the floor to get past this."

CREATED_CLUSTER=false
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
CREATED_CLUSTER=true

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

# 3. containerd registry wiring, per node.
#
# ============================ fbiy-C1 EXTENSION HOOK =========================
# C1 (prove the governor end to end) needs a node that can actually run the
# writable-cgroup RuntimeClass. Everything it must add is local to this
# function plus one node label:
#
#   * write a `runc-cgroupwritable` runtime handler into the node's
#     /etc/containerd/config.toml (plugins."io.containerd.grpc.v1.cri".containerd
#     .runtimes.runc-cgroupwritable) and restart containerd on the node;
#   * `kubectl label node <node> djinn.io/cgroup-writable=true`;
#   * flip cgroupWritable.runtimeClass.enabled / cgroupWritable.taskRuns.enabled
#     and cgroupLauncher.mode in a C1-owned values file.
#
# It must NOT do that here. `deploy/kueue/preflight.sh --mode cutover` exiting
# 10 against this cluster is 6knv's AC4, and exit 10 means "RuntimeClass
# djinn-cgroup-writable is absent". Installing the class in B0 would delete
# that assertion without deleting a single line of the test that makes it.
# =============================================================================
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
    done
}
info 'wiring containerd registry mirrors (registries only — see the fbiy-C1 hook)'
configure_node_containerd

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
