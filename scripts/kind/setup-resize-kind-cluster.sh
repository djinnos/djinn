#!/usr/bin/env bash
# A DISPOSABLE kind cluster for the in-place task-run resize proof (3i92 / pcod).
#
# WHAT THIS IS FOR
# ----------------
# `server/tests/task_run_resize_kind.rs` asks questions no rendered fixture can
# answer: does a real `shell` brokered through the production Unix broker land
# in the `cgroup-launcher` sidecar's cgroup hierarchy, does the `pods/resize`
# subresource move that sidecar's CPU limit IN PLACE, and does the kernel then
# spend the CPU the resize authorized. Every one of those needs an API server
# and a node with a writable cgroup mount. This script produces exactly one of
# each, and nothing else.
#
# It is the enabling half; the assertions live in the Rust file.
#
# THIS MUST NEVER ARM PRODUCTION
# ------------------------------
# Nothing here is a step toward the production VPS. Arming the launcher there is
# a gated operator act behind `deploy/kueue/preflight.sh --mode cutover` and
# belongs to epic 4c9q's runbook. Five guards keep this script off every target
# it does not own, and all of them run BEFORE any cluster, registry or
# kubeconfig is touched (`check` runs them and stops):
#
#   1. The cluster, its context, its registry and its registry port all carry
#      harness-specific names. The developer's Tilt cluster (`kind-djinn`,
#      `kind-registry`, port 5001) and every SIBLING kind harness in this
#      repository are REFUSED names, not merely unlikely ones (exit 3). A
#      sibling agent may be running its cluster concurrently; colliding on a
#      registry port would either fail to bind or shadow theirs.
#   2. A caller-supplied `--context` that is not the context this script is
#      about to create is refused (exit 3). It is never "used anyway". Every
#      context in a Djinn developer's kubeconfig today is an EKS cluster, so
#      "whatever is current" is the single worst default available; this script
#      never reads the current context.
#   3. Every `kubectl` and `helm` invocation below is pinned with
#      `--context` / `--kube-context`. There is no unpinned call.
#   4. `up` refuses to adopt a kind cluster or a registry container it did not
#      create (exit 6). A cluster this script did not create is one whose
#      contents it cannot vouch for, and tearing it down on failure would then
#      destroy someone else's work.
#   5. Free space on `/` is measured before the first image is pulled (exit 4).
#
# KUBERNETES FLOORS: 1.30 GENERALLY, 1.33 FOR `pods/resize`
# ---------------------------------------------------------
# Two floors, both exit 7, and they are different facts:
#
#   * 1.30 is the repository floor. Kueue 0.19.0 requires it and `k8s-openapi`
#     is compiled against `v1_30` in `server/crates/djinn-k8s/Cargo.toml`. The
#     1.29 floor that used to be documented was MEASURED FALSE on 2026-07-30 and
#     corrected in #2818 — do not restore it.
#   * 1.33 is this harness's own floor, because `pods/resize` is the
#     subresource the whole proof rests on and it does not exist below it
#     (`deploy/helm/djinn/templates/role-controller.yaml` grants `pods/resize`
#     for the same reason). A cluster between the two floors would let the
#     Pod render, admit and run while every PATCH 404s.
#
# THE WRITABLE-CGROUP NODE IS MANDATORY HERE
# ------------------------------------------
# `scripts/kind/setup-kueue-cluster.sh` makes the writable-cgroup node opt-in
# (`--cgroup-writable`) because its B0/B1/B2 clusters must stay byte identical
# to what they were measured against. This harness has no such constraint and
# the opposite requirement: the launcher's `cgroup.kill`, its leaf creation and
# every `cpu.stat` read in the proof need a delegated, writable
# `/sys/fs/cgroup`. So the node handler and the node label are installed
# unconditionally, and there is deliberately NO flag to turn them off. A run
# without them would not fail — it would pass a weaker test under plain `runc`.
#
# Two node-level facts are installed, and NOTHING chart-level:
#
#   * a `runc-cgroupwritable` containerd runtime handler on every node, written
#     into the schema the node's LIVE `/etc/containerd/config.toml` declares
#     (resolved with `deploy/node/k3s/containerd-config-version.sh`, the same
#     detector the managed-k3s conformance uses — never from a version string).
#     `kindest/node` ships containerd BINARY v2.2.0 under a config declaring
#     `version = 2`, while the production VPS runs v3; a handler written into
#     the wrong table is accepted SILENTLY, the RuntimeClass still resolves, the
#     Pod still runs — under plain `runc` with a READ-ONLY /sys/fs/cgroup. So
#     the result is verified with `crictl info` rather than assumed.
#   * the `djinn.io/cgroup-writable=true` node label the RuntimeClass's
#     `scheduling.nodeSelector` requires.
#
# The RuntimeClass object itself is the CHART's, gated by
# `cgroupWritable.runtimeClass.enabled`, so the default values file here is the
# one that enables it.
#
# USAGE
#   scripts/kind/setup-resize-kind-cluster.sh up        # create + install + arm
#   scripts/kind/setup-resize-kind-cluster.sh down      # delete cluster AND registry
#   scripts/kind/setup-resize-kind-cluster.sh check     # guards only, touch nothing
#   scripts/kind/setup-resize-kind-cluster.sh selftest  # prove the teardown trap fires
#
# OPTIONS
#   --context <ctx>          refused unless it equals the derived context
#   --cluster-name <name>    default djinn-resize-pcod
#   --registry-name <name>   default djinn-resize-pcod-registry
#   --registry-port <port>   default 5067
#   --k8s-version <x.y.z>    default 1.35.0
#   --values <file>          default deploy/helm/djinn/tests/fixtures/kueue-governor-values.yaml
#   --min-free-gib <n>       default 25
#   --keep-on-failure        leave the cluster up when `up` fails
#
# EXIT CODES
#   0   ok
#   2   usage / caller error
#   3   refused a context or a reserved name
#   4   not enough free disk
#   5   a required tool is missing
#   6   refused to adopt a pre-existing cluster or registry
#   7   Kubernetes below one of the two floors
#   1   anything else
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

KIND="${KIND:-kind}"
KUBECTL_BIN="${KUBECTL:-kubectl}"
HELM_BIN="${HELM:-helm}"
DOCKER="${DOCKER:-docker}"

# Harness-specific everything. None of these may collide with the Tilt cluster
# or with any sibling kind harness; see RESERVED_* below.
CLUSTER_NAME="${DJINN_RESIZE_HARNESS_CLUSTER:-djinn-resize-pcod}"
REG_NAME="${DJINN_RESIZE_HARNESS_REGISTRY:-djinn-resize-pcod-registry}"
REG_PORT="${DJINN_RESIZE_HARNESS_REG_PORT:-5067}"
KIND_IMAGE_VERSION="${DJINN_RESIZE_HARNESS_K8S_VERSION:-1.35.0}"
MIN_FREE_GIB="${DJINN_RESIZE_HARNESS_MIN_FREE_GIB:-25}"
INSTALL_TIMEOUT_SECONDS="${DJINN_RESIZE_HARNESS_INSTALL_TIMEOUT_SECONDS:-600}"
VALUES_FILE="${DJINN_RESIZE_HARNESS_VALUES:-$REPO_ROOT/deploy/helm/djinn/tests/fixtures/kueue-governor-values.yaml}"
REQUESTED_CONTEXT=""
KEEP_ON_FAILURE=false
ACTION=""

# Test-only failure injection, consumed by `selftest`. Named values only; an
# unrecognised value is a usage error rather than a silent no-op, because a
# self-test whose injection point stopped existing would otherwise report PASS.
FAIL_AFTER="${DJINN_RESIZE_HARNESS_FAIL_AFTER:-}"

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
# The throwaway object step 4a admits to prove the Kueue webhook is serving. Its
# own name: it is created and deleted seconds apart, and must never be mistaken
# for the chart's `djinn-kueue` ResourceFlavor.
WEBHOOK_PROBE_FLAVOR="djinn-resize-harness-webhook-probe"
WEBHOOK_PROBE_ATTEMPTS="${DJINN_RESIZE_HARNESS_WEBHOOK_ATTEMPTS:-90}"
WEBHOOK_PROBE_INTERVAL_SECONDS=2
CHART="$REPO_ROOT/deploy/helm/djinn"
RELEASE="djinn"
NAMESPACE="djinn"

# The names this script must never touch, whatever it is asked to do.
#
# `djinn` / `kind-registry` / 5000 / 5001 are `scripts/kind/setup-kind.sh`'s
# defaults, i.e. the developer's live Tilt environment. The rest are the SIBLING
# live-cluster harnesses in this repository. They are spelled out as literals
# rather than imported, because a sibling agent may be running one of them right
# now and "we both derive our names from the same constant" is not isolation.
RESERVED_CLUSTER_NAMES=(
    djinn
    kind
    djinn-validation
    djinn-kueue-harness
    djinn-kueue-b1
    djinn-kueue-b2
    djinn-kueue-b2b
    djinn-kueue-c1
)
RESERVED_REGISTRY_NAMES=(
    kind-registry
    kind-registry-validation
    djinn-kueue-harness-registry
    djinn-kueue-b1-registry
    djinn-kueue-b2-registry
    djinn-kueue-b2b-registry
    djinn-kueue-c1-registry
)
RESERVED_REG_PORTS=(5000 5001 5051 5053 5055 5061 15001)

EXIT_USAGE=2
EXIT_REFUSED_TARGET=3
EXIT_LOW_DISK=4
EXIT_MISSING_TOOL=5
EXIT_CLUSTER_EXISTS=6
EXIT_VERSION_FLOOR=7

# The repository floor (Kueue 0.19 / k8s-openapi v1_30). Corrected from a
# measured-false 1.29 in #2818.
MIN_K8S_MINOR=30
# This harness's own floor: `pods/resize` does not exist below 1.33.
RESIZE_MIN_K8S_MINOR=33

# The header comment above IS the help text. Printing it rather than
# maintaining a second copy is why the range stops at the last commented line,
# which `awk` finds instead of a line number that would silently drift.
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
        up|down|check|selftest)
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

case "$FAIL_AFTER" in
    ''|registry|cluster) ;;
    *) fail "$EXIT_USAGE" "DJINN_RESIZE_HARNESS_FAIL_AFTER must be empty, 'registry' or 'cluster', got: $FAIL_AFTER" ;;
esac

# --- Guards (all of them run before anything is created) --------------------

[[ "$CLUSTER_NAME" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] \
    || fail "$EXIT_USAGE" "--cluster-name must be a DNS label, got: $CLUSTER_NAME"
[[ "$REG_PORT" =~ ^[1-9][0-9]{2,4}$ ]] \
    || fail "$EXIT_USAGE" "--registry-port must be a TCP port, got: $REG_PORT"
[[ "$MIN_FREE_GIB" =~ ^[0-9]+$ ]] \
    || fail "$EXIT_USAGE" "--min-free-gib must be a non-negative integer, got: $MIN_FREE_GIB"

# Guard 1: reserved names. Neither the Tilt cluster nor any sibling harness is
# "unlikely to be selected"; they are unselectable.
for reserved in "${RESERVED_CLUSTER_NAMES[@]}"; do
    [ "$CLUSTER_NAME" != "$reserved" ] || fail "$EXIT_REFUSED_TARGET" \
        "refusing to operate on kind cluster '$CLUSTER_NAME': it belongs to the developer's Tilt environment or to a sibling live-cluster harness. This harness creates and DELETES its target, so it must own the name outright."
done
for reserved in "${RESERVED_REGISTRY_NAMES[@]}"; do
    [ "$REG_NAME" != "$reserved" ] || fail "$EXIT_REFUSED_TARGET" \
        "refusing to operate on registry '$REG_NAME': it belongs to the Tilt environment or to a sibling harness, and 'down' deletes the registry it is given."
done
for reserved in "${RESERVED_REG_PORTS[@]}"; do
    [ "$REG_PORT" != "$reserved" ] || fail "$EXIT_REFUSED_TARGET" \
        "refusing registry port $REG_PORT: it is published by the Tilt registry or by a sibling harness that may be running right now, and binding it would either fail or shadow theirs."
done

# Guard 2: the two Kubernetes floors, checked on the REQUESTED image before a
# cluster exists, and re-checked against the live API server after create.
K8S_MAJOR=${KIND_IMAGE_VERSION%%.*}
K8S_REST=${KIND_IMAGE_VERSION#*.}
K8S_MINOR=${K8S_REST%%.*}
[[ "$K8S_MAJOR" =~ ^[0-9]+$ && "$K8S_MINOR" =~ ^[0-9]+$ ]] \
    || fail "$EXIT_USAGE" "--k8s-version must look like 1.35.0, got: $KIND_IMAGE_VERSION"
if [ "$K8S_MAJOR" -lt 1 ] || { [ "$K8S_MAJOR" -eq 1 ] && [ "$K8S_MINOR" -lt "$MIN_K8S_MINOR" ]; }; then
    fail "$EXIT_VERSION_FLOOR" \
        "Kubernetes $KIND_IMAGE_VERSION is below the 1.$MIN_K8S_MINOR repository floor (Kueue 0.19.0, k8s-openapi v1_30). The 1.29 floor this repository used to document was measured false on 2026-07-30 and corrected in #2818; do not restore it."
fi
if [ "$K8S_MAJOR" -lt 1 ] || { [ "$K8S_MAJOR" -eq 1 ] && [ "$K8S_MINOR" -lt "$RESIZE_MIN_K8S_MINOR" ]; }; then
    fail "$EXIT_VERSION_FLOOR" \
        "Kubernetes $KIND_IMAGE_VERSION has no pods/resize subresource; this harness needs 1.$RESIZE_MIN_K8S_MINOR or newer. Below it the Pod renders, admits and runs while every resize PATCH 404s, which reads as 'the launcher was never resized' rather than 'this cluster cannot resize'."
fi

# Guard 3: the context. Derived, never discovered — this script does not read
# the current context, because on a Djinn developer's machine every context in
# the kubeconfig is a live EKS cluster.
CONTEXT="kind-${CLUSTER_NAME}"
if [ -n "$REQUESTED_CONTEXT" ] && [ "$REQUESTED_CONTEXT" != "$CONTEXT" ]; then
    fail "$EXIT_REFUSED_TARGET" \
        "refusing --context '$REQUESTED_CONTEXT': this harness only ever targets '$CONTEXT', the context of the kind cluster it creates and deletes itself. An externally supplied context is a cluster whose contents this script cannot vouch for — and every context in a Djinn developer's kubeconfig today is a live EKS cluster (demo/staging/prod)."
fi

KUBECTL=("$KUBECTL_BIN" --context "$CONTEXT")
HELM=("$HELM_BIN" --kube-context "$CONTEXT")

if [ "$ACTION" = check ]; then
    printf 'PASS: guards accept cluster=%s context=%s registry=%s:%s k8s=%s resize-floor=1.%s\n' \
        "$CLUSTER_NAME" "$CONTEXT" "$REG_NAME" "$REG_PORT" "$KIND_IMAGE_VERSION" "$RESIZE_MIN_K8S_MINOR"
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

# --- selftest ---------------------------------------------------------------
#
# The trap is the only thing standing between "this harness failed" and "this
# host now has an orphaned kind cluster and a registry container holding a
# port". A comment claiming the trap exists is not evidence, and neither is a
# grep: `trap on_exit EXIT` installed AFTER the registry is started would still
# leave the registry behind. So this drives a REAL `up` that is injected to fail
# immediately after the registry container exists, and then proves the container
# is gone. It stops before `kind create cluster`, so it costs one 25MB registry
# image and about two seconds.
if [ "$ACTION" = selftest ]; then
    # Its OWN names and its OWN port. The self-test must be runnable while the
    # real harness cluster is up — which is exactly when someone runs the suite
    # — and `up` refuses to adopt a pre-existing registry (guard 4), so reusing
    # the live names would fail before the trap was ever installed and report
    # "the registry survived" for the wrong reason.
    SELFTEST_CLUSTER="${CLUSTER_NAME}-selftest"
    SELFTEST_REGISTRY="${REG_NAME}-selftest"
    SELFTEST_PORT=$((REG_PORT + 1))
    info "selftest: running 'up' with a failure injected after the registry is created (${SELFTEST_REGISTRY}:${SELFTEST_PORT})"
    # Never adopt: if a previous self-test crashed hard, say so rather than
    # deleting a container this run did not create.
    if "$DOCKER" inspect "$SELFTEST_REGISTRY" >/dev/null 2>&1; then
        fail 1 "selftest: ${SELFTEST_REGISTRY} already exists; remove it before re-running"
    fi
    set +e
    DJINN_RESIZE_HARNESS_FAIL_AFTER=registry "$0" up \
        --cluster-name "$SELFTEST_CLUSTER" \
        --registry-name "$SELFTEST_REGISTRY" \
        --registry-port "$SELFTEST_PORT" \
        --k8s-version "$KIND_IMAGE_VERSION" \
        --values "$VALUES_FILE" \
        --min-free-gib "$MIN_FREE_GIB" >/dev/null 2>&1
    injected=$?
    set -e
    [ "$injected" -ne 0 ] || fail 1 \
        "selftest: the injected failure did not fail the run; DJINN_RESIZE_HARNESS_FAIL_AFTER is no longer wired"
    if "$DOCKER" inspect "$SELFTEST_REGISTRY" >/dev/null 2>&1; then
        "$DOCKER" rm -f "$SELFTEST_REGISTRY" >/dev/null || true
        fail 1 "selftest: registry container ${SELFTEST_REGISTRY} SURVIVED a failed 'up'. The teardown trap is missing, or it is installed after the registry is created."
    fi
    if "$KIND" get clusters 2>/dev/null | grep -qx "$SELFTEST_CLUSTER"; then
        fail 1 "selftest: kind cluster ${SELFTEST_CLUSTER} survived a failed 'up'"
    fi
    printf 'PASS: the teardown trap removed the registry after an injected failure (cluster=%s registry=%s)\n' \
        "$SELFTEST_CLUSTER" "$SELFTEST_REGISTRY"
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

# Guard 5: disk. A kind node, a Kueue install and a probe image are not small
# things to discover you cannot afford halfway through.
#
# An unparseable `df` is a failure, not zero free space and not a skip. Both of
# the wrong answers are worse than stopping.
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
# Installed BEFORE the registry container exists. Everything this script creates
# from here on is created under the trap.
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

[ "$FAIL_AFTER" != registry ] || fail 1 "injected failure after the registry (DJINN_RESIZE_HARNESS_FAIL_AFTER=registry)"

# 2. The cluster.
#
# `InPlacePodVerticalScaling` is named explicitly rather than left to the
# release default. It is the gate the `pods/resize` subresource hangs off, and a
# node image that shipped it off by default would produce exactly the failure
# the 1.33 floor above exists to prevent — a 404 on every PATCH.
info "creating kind cluster ${CLUSTER_NAME} at Kubernetes v${KIND_IMAGE_VERSION}"
"$KIND" create cluster --name "$CLUSTER_NAME" --image "kindest/node:v${KIND_IMAGE_VERSION}" --config=- <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
featureGates:
  InPlacePodVerticalScaling: true
containerdConfigPatches:
  - |-
    [plugins."io.containerd.grpc.v1.cri".registry]
      config_path = "/etc/containerd/certs.d"
nodes:
  - role: control-plane
EOF

[ "$FAIL_AFTER" != cluster ] || fail 1 "injected failure after the cluster (DJINN_RESIZE_HARNESS_FAIL_AFTER=cluster)"

# `kind create cluster` switches the current context to the new cluster. Every
# call below is still pinned; this is only so a caller left on the harness
# context is left on a context that exists.
info "created context ${CONTEXT} (current-context is now this disposable cluster)"

# Re-check both floors against the LIVE API server. The kind node image tag is a
# claim; `kubectl version` is the measurement. This is the same distinction that
# made the documented 1.29 floor wrong for five minor versions.
SERVER_MINOR=$("${KUBECTL[@]}" version -o json | sed -n 's/.*"minor": *"\([0-9]*\)".*/\1/p' | tail -1)
[[ "$SERVER_MINOR" =~ ^[0-9]+$ ]] || fail 1 "could not read the live Kubernetes minor version from context $CONTEXT"
[ "$SERVER_MINOR" -ge "$MIN_K8S_MINOR" ] || fail "$EXIT_VERSION_FLOOR" \
    "the live API server reports 1.${SERVER_MINOR}, below the 1.${MIN_K8S_MINOR} repository floor"
[ "$SERVER_MINOR" -ge "$RESIZE_MIN_K8S_MINOR" ] || fail "$EXIT_VERSION_FLOOR" \
    "the live API server reports 1.${SERVER_MINOR}, below the 1.${RESIZE_MIN_K8S_MINOR} floor the pods/resize subresource requires"
info "live API server is Kubernetes 1.${SERVER_MINOR} (floors 1.${MIN_K8S_MINOR} repository, 1.${RESIZE_MIN_K8S_MINOR} pods/resize)"

# 3. containerd wiring, per node: registry mirrors, and the
#    `runc-cgroupwritable` runtime handler the RuntimeClass names.
#
# WHY THE SCHEMA IS MEASURED AND NOT ASSUMED
# ------------------------------------------
# A kind node's containerd BINARY version and its CRI CONFIGURATION schema are
# different facts, and on this image they disagree: `kindest/node:v1.35.0` ships
# containerd 2.2.0 — which reads schema v3 — under a `/etc/containerd/config.toml`
# whose first non-comment line is literally `version = 2`. The production VPS
# runs the v3 schema (`deploy/node/k3s/containerd/config-v3.toml.tmpl`), so a
# handler block copied from the repo's production asset would land in a plugin
# namespace this node's config does not use and would be silently ignored.
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

# Added by scripts/kind/setup-resize-kind-cluster.sh (3i92 / pcod).
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

info 'wiring containerd registry mirrors and the writable-cgroup handler'
configure_node_containerd

# The label half of the contract. `RuntimeClass/djinn-cgroup-writable` carries
# `scheduling.nodeSelector: djinn.io/cgroup-writable: "true"`, which the
# RuntimeClass admission controller merges into every Pod naming the class — so
# without this an armed task-run Pod is permanently Pending with no event that
# mentions cgroups at all.
for node in $("$KIND" get nodes --name "$CLUSTER_NAME"); do
    info "labelling node ${node} ${CGROUP_WRITABLE_NODE_LABEL}=true"
    "${KUBECTL[@]}" label node "$node" "${CGROUP_WRITABLE_NODE_LABEL}=true" --overwrite
done
# The API server and the kubelet both went through a containerd restart; wait
# for the node to be Ready again before helm starts creating objects.
"${KUBECTL[@]}" wait --for=condition=Ready node --all --timeout=180s

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

# 4a. `--wait` is necessary and NOT sufficient. It returns when the controller's
# Pod reports Ready, and "Ready" is a strictly weaker fact than "the webhook is
# serving": kueue answers its readiness probe on the health port before the
# webhook server has bound :9443. Installing the djinn chart inside that window
# fails on whichever webhook the API server reaches first — observed twice while
# verifying this script, roughly one bring-up in two:
#
#   Error: failed to create resource: Internal error occurred: conversion webhook
#   for kueue.x-k8s.io/v1beta1, Kind=ClusterQueue failed: Post
#   "https://djinn-prereqs-kueue-webhook-service.kueue-system.svc:443/convert?
#   timeout=30s": dial tcp 10.96.219.155:443: connect: connection refused
#
#   Error: ... failed calling webhook "mresourceflavor.kb.io" ... connection refused
#
# Both are the same fact and both land in the same place the namespace collision
# did: the bring-up step, with zero test assertions executed. So this is a
# MEASUREMENT rather than a sleep or a condition read off the Deployment. It
# creates a throwaway `v1beta1` ResourceFlavor, which the API server can only
# admit by calling BOTH the conversion webhook (v1beta1 is not the storage
# version) and `mresourceflavor.kb.io`, on the same :9443 the chart's topology
# needs. When that round trip succeeds, the webhook is serving.
info 'waiting for the Kueue webhook to actually serve (Pod Ready is a weaker fact)'
webhook_probe_apply() {
    "${KUBECTL[@]}" apply -f - 2>&1 <<EOF
apiVersion: kueue.x-k8s.io/v1beta1
kind: ResourceFlavor
metadata:
  name: ${WEBHOOK_PROBE_FLAVOR}
EOF
}
webhook_serving=false
webhook_last_output=""
webhook_attempt=0
while [ "$webhook_attempt" -lt "$WEBHOOK_PROBE_ATTEMPTS" ]; do
    webhook_attempt=$((webhook_attempt + 1))
    if webhook_last_output=$(webhook_probe_apply); then
        webhook_serving=true
        break
    fi
    sleep "$WEBHOOK_PROBE_INTERVAL_SECONDS"
done
"${KUBECTL[@]}" delete resourceflavor "$WEBHOOK_PROBE_FLAVOR" --ignore-not-found >/dev/null 2>&1 || true
[ "$webhook_serving" = true ] || fail 1 \
    "the Kueue webhook never served after ${webhook_attempt} attempts over $((WEBHOOK_PROBE_ATTEMPTS * WEBHOOK_PROBE_INTERVAL_SECONDS))s, so the djinn chart's queue topology cannot be applied. Last error:
${webhook_last_output}"
info "Kueue webhook is serving (admitted the probe ResourceFlavor on attempt ${webhook_attempt})"

# 5. WHO OWNS THE `djinn` NAMESPACE.
#
# The chart does — and this block exists so that helm agrees with it. Read this
# before touching either the `kubectl apply` or the helm flags below; the two are
# one mechanism and changing either alone reintroduces a wedge that produces ZERO
# test assertions.
#
# `$VALUES_FILE` sets `namespace.create: true` (the chart default), so
# `deploy/helm/djinn/templates/namespace.yaml` renders a Namespace object as part
# of the release, and it is that object that carries `djinn.io/kueue-managed`.
# The label is not decoration: Kueue captures Jobs ONLY in a namespace carrying
# it, and the chart refuses to arm without it. So the chart must stay the thing
# that applies it.
#
# Helm 3 then boxes this in from both sides, and BOTH sides were observed:
#
#   * `--create-namespace` (what this script used to pass) makes helm create the
#     namespace itself, out-of-band, with no release ownership metadata. The
#     release's own rendered Namespace is then a second create of the same
#     object:
#
#         Release "djinn" does not exist. Installing it now.
#         Error: 1 error occurred:
#           * namespaces "djinn" already exists
#
#     That is verbatim what killed the first-ever dispatch of
#     `.github/workflows/resize-kind.yml` — both jobs, at this step, with zero
#     test assertions executed. `--take-ownership` does NOT rescue it: it relaxes
#     the ownership CHECK, not the create.
#
#   * Dropping the flag and letting the chart render the namespace unaided fails
#     one step earlier, because helm stores the release record IN the release
#     namespace and needs it to exist before any manifest is applied:
#
#         Error: create: failed to create: namespaces "djinn" not found
#
# The way through is helm's own documented resource-adoption contract: a
# pre-existing object carrying the managed-by label and the two `meta.helm.sh`
# annotations is ADOPTED into the release instead of being created. So this
# script creates an EMPTY namespace shell stamped with exactly those three keys
# and nothing else, and the chart's Namespace object — labels, `djinn.io/
# kueue-managed` and all — lands on top of it as a release-owned update. Ownership
# ends where it started: with the chart. Verified against helm 3.21.3 (what the
# workflow's `get-helm-3` installs) and helm 4.2.1.
#
# Omit any of the three keys and helm 3 names the missing one:
#
#     Error: Unable to continue with install: Namespace "djinn" in namespace ""
#     exists and cannot be imported into the current release: invalid ownership
#     metadata; label validation error: missing key
#     "app.kubernetes.io/managed-by": must be set to "Helm"; ...
#
# The prerequisite install above KEEPS `--create-namespace` for the mirror-image
# reason: `djinn-prereqs` renders no Namespace object, so there helm is the only
# candidate owner and `kueue-system` would otherwise never exist.
#
# The alternative — pre-creating the namespace with `--set namespace.create=false`
# — is rejected on purpose. The chart `fail`s that render outright while armed,
# and silencing THAT with `kueue.namespaceLabelledExternally=true` would arm this
# cluster down a different branch than production, with `djinn.io/kueue-managed`
# applied by this script instead of by the chart. The proof would stop exercising
# the thing it exists to exercise.
info "creating the ${NAMESPACE} namespace shell for helm to adopt into release ${RELEASE}"
"${KUBECTL[@]}" apply -f - <<EOF
apiVersion: v1
kind: Namespace
metadata:
  name: ${NAMESPACE}
  labels:
    app.kubernetes.io/managed-by: Helm
  annotations:
    meta.helm.sh/release-name: ${RELEASE}
    meta.helm.sh/release-namespace: ${NAMESPACE}
EOF

# 6. The djinn chart, ARMED.
#
# Deliberately NOT `--wait`. The harness proves a resize contract, not a running
# Djinn: the server image tag in the values file resolves to nothing here and
# its Deployment is expected to sit in ImagePullBackOff forever. Waiting would
# time out on a state that is by design.
info "installing djinn chart release ${RELEASE} (RuntimeClass + cgroup launcher armed)"
"${HELM[@]}" upgrade --install "$RELEASE" "$CHART" \
    --namespace "$NAMESPACE" \
    --values "$VALUES_FILE" \
    --set kueue.enabled=true --set kueue.armed=true

# The adoption actually happened. This is a BRING-UP precondition, not a
# restatement of the resize contract: if the chart's Namespace object did not
# land on the shell above, `djinn.io/kueue-managed` is absent, Kueue captures
# nothing, every suspended Job hangs forever, and the Rust suite reports it as
# "no Pod reached Running" — a scheduling story for a labelling fault. Fail here,
# where the cause is still legible.
kueue_managed=$("${KUBECTL[@]}" get namespace "$NAMESPACE" \
    -o 'jsonpath={.metadata.labels.djinn\.io/kueue-managed}')
[ "$kueue_managed" = true ] || fail 1 \
    "namespace ${NAMESPACE} is not labelled djinn.io/kueue-managed=true after the armed install (read back: '${kueue_managed}'). The chart's Namespace object did not adopt the shell this script created, so nothing would ever unsuspend a captured Job."

# 7. Report what the API server actually holds. Printed, not asserted: the
# assertions live in server/tests/task_run_resize_kind.rs, and duplicating them
# here in a weaker form would invite someone to trust the weaker copy.
info 'cluster state:'
"${KUBECTL[@]}" get runtimeclass djinn-cgroup-writable \
    -o 'jsonpath={"runtimeclass "}{.metadata.name}{" handler="}{.handler}{"\n"}' || true
"${KUBECTL[@]}" get nodes \
    -o 'jsonpath={range .items[*]}{"node "}{.metadata.name}{" cgroup-writable="}{.metadata.labels.djinn\.io/cgroup-writable}{"\n"}{end}'
"${KUBECTL[@]}" get namespace "$NAMESPACE" \
    -o 'jsonpath={.metadata.name}{" djinn.io/kueue-managed="}{.metadata.labels.djinn\.io/kueue-managed}{"\n"}'

cat <<EOF

>>> disposable resize-proof cluster '${CLUSTER_NAME}' is ready.

    context:   ${CONTEXT}
    registry:  ${REG_NAME} (127.0.0.1:${REG_PORT})

Run the harness:

    DJINN_TEST_RESIZE_KIND=1 cargo test -p djinn-server --test task_run_resize_kind -- --ignored --test-threads=1

TEAR IT DOWN when you are finished — cluster AND registry:

    $0 down --cluster-name ${CLUSTER_NAME} --registry-name ${REG_NAME}
EOF
