#!/usr/bin/env bash
# Disposable kind cluster for the mixed-version launcher-authority matrix (omp4).
#
# This script CREATES AND DELETES its target. Nothing about that target is
# discovered: the cluster name, the registry name, the registry port and the
# context are all derived from the constants below and never read out of the
# ambient kubeconfig. On a Djinn developer's machine every context in the
# kubeconfig is a live EKS cluster, so "use the current context" is not a
# convenience here — it is a production outage.
#
# WHY IT DELEGATES INSTEAD OF REBUILDING
#
# scripts/kind/setup-kueue-cluster.sh already knows how to stand up a node whose
# containerd carries the `runc-cgroupwritable` handler and whose kubelet is
# labelled for it (#2857), and how to install the djinn chart's ServiceAccount,
# PVCs and RuntimeClass. Re-deriving any of that here would give the matrix a
# SECOND opinion about how a cgroup-writable node is built, and the two would
# drift. So `up` runs the guards this harness owns, then hands the creation to
# that script with THIS harness's names, and adds only what the matrix itself
# needs on top. Teardown is NOT delegated: a disposable harness must be able to
# prove its own target is gone without depending on another script's exit path.
#
# WHY A SEPARATE CLUSTER FROM EVERY SIBLING
#
# The matrix flips a server-wide launcher authority mode and dispatches Pods it
# expects to be REFUSED. Both are cluster-visible side effects that would
# corrupt a concurrent harness's accounting, and the reserved lists below name
# every sibling so the two can never fight over a cluster or a port.
#
# Usage:
#   scripts/kind/setup-resize-matrix-cluster.sh up         # create (default)
#   scripts/kind/setup-resize-matrix-cluster.sh down       # delete cluster AND registry
#   scripts/kind/setup-resize-matrix-cluster.sh check      # every guard, create nothing
#   scripts/kind/setup-resize-matrix-cluster.sh selfcheck  # prove teardown really tears down
#
# Options:
#   --context NAME         must equal the derived context; any other value is refused
#   --cluster-name NAME    default djinn-resize-omp4
#   --registry-name NAME   default djinn-resize-omp4-registry
#   --registry-port PORT   default 5071
#   --k8s-version VER      default 1.35.0
#   --min-free-gib N       default 30
#   --keep-on-failure      skip the failure-path teardown (debugging only)
#
# Exit codes:
#   2   usage error
#   3   refused target (reserved name, a non-derived context, or a non-kind server)
#   4   not enough free disk
#   5   a required tool is missing
#   6   refused to adopt a pre-existing cluster
#   7   Kubernetes version below the floor
#   1   anything else
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

KIND="${KIND:-kind}"
KUBECTL_BIN="${KUBECTL:-kubectl}"
DOCKER="${DOCKER:-docker}"

CLUSTER_NAME="${DJINN_RESIZE_MATRIX_CLUSTER:-djinn-resize-omp4}"
REG_NAME="${DJINN_RESIZE_MATRIX_REGISTRY:-djinn-resize-omp4-registry}"
REG_PORT="${DJINN_RESIZE_MATRIX_REG_PORT:-5071}"
KIND_IMAGE_VERSION="${DJINN_RESIZE_MATRIX_K8S_VERSION:-1.35.0}"
MIN_FREE_GIB="${DJINN_RESIZE_MATRIX_MIN_FREE_GIB:-30}"
REQUESTED_CONTEXT=""
KEEP_ON_FAILURE=false
ACTION=""

# The delegate. Named as a constant so the Rust suite's guard can assert this
# harness reuses it rather than growing a second cgroup-writable installer.
UPSTREAM_SETUP="$REPO_ROOT/scripts/kind/setup-kueue-cluster.sh"

# Everything this script must never touch, whatever it is asked to do.
#
# `djinn` / `kind-registry` / 5001 are scripts/kind/setup-kind.sh's defaults —
# the developer's live Tilt environment. The rest are the OTHER disposable
# harnesses; this script deletes what it is given, and a `down` aimed at one of
# them would destroy a run in progress.
RESERVED_CLUSTER_NAMES=(
    djinn
    kind
    djinn-kueue-harness
    djinn-kueue-b2
    djinn-kueue-b2b
    djinn-kueue-c1
    djinn-resize-harness
    djinn-resize-pcod
)
RESERVED_REGISTRY_NAMES=(
    kind-registry
    djinn-kueue-harness-registry
    djinn-kueue-c1-registry
    djinn-resize-harness-registry
    djinn-resize-pcod-registry
)
RESERVED_REG_PORTS=(5000 5001 5051 5052 5055 5061 5067)

EXIT_USAGE=2
EXIT_REFUSED_TARGET=3
EXIT_LOW_DISK=4
EXIT_MISSING_TOOL=5
EXIT_CLUSTER_EXISTS=6
EXIT_VERSION_FLOOR=7

# The floor.
#
# The EPIC's floor is Kubernetes 1.30 — Kueue 0.19 requires it, and the "1.29"
# that appeared in older docs was MEASURED FALSE and corrected in #2818. Do not
# restore 1.29 here or anywhere.
#
# This harness's own floor is higher, and both bounds are enforced: `pods/resize`
# must exist and must actuate (`InPlacePodVerticalScaling`, beta and default-on
# from 1.33), and `--cgroup-writable` needs a containerd that carries the
# `cgroup_writable` runtime property, which the 1.31 node image does not ship.
# A harness that silently ran without either would report "not confirmed" for a
# reason that has nothing to do with the code under test.
EPIC_MIN_K8S_MINOR=30
MIN_K8S_MINOR=33

# A floor below the epic's is a regression whatever the local reason. Asserted
# here so an edit to MIN_K8S_MINOR cannot quietly undo #2818.
[ "$MIN_K8S_MINOR" -ge "$EPIC_MIN_K8S_MINOR" ] || {
    printf 'FAIL: MIN_K8S_MINOR=%s is below the epic floor 1.%s (see #2818)\n' \
        "$MIN_K8S_MINOR" "$EPIC_MIN_K8S_MINOR" >&2
    exit 1
}

# The chart-installed pieces the matrix renders against, and the node-side
# directory the AC4 sentinel lands in.
NAMESPACE="${DJINN_RESIZE_MATRIX_NAMESPACE:-djinn}"
SENTINEL_DIR="/var/tmp/djinn-resize-matrix-sentinels"
CGROUP_WRITABLE_RUNTIME_CLASS="djinn-cgroup-writable"

# The header comment above IS the help text.
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

require_tool() {
    command -v "$1" >/dev/null 2>&1 || fail "$EXIT_MISSING_TOOL" "$1 is not on PATH"
}

# --- Argument parsing -------------------------------------------------------
while [ "$#" -gt 0 ]; do
    case "$1" in
        up|down|check|selfcheck)
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
        --min-free-gib)
            [ "$#" -ge 2 ] || fail "$EXIT_USAGE" '--min-free-gib requires a value'
            MIN_FREE_GIB=$2
            shift 2
            ;;
        --keep-on-failure)
            KEEP_ON_FAILURE=true
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage
            fail "$EXIT_USAGE" "unknown argument: $1"
            ;;
    esac
done
ACTION="${ACTION:-up}"

# --- Guard 1: shapes --------------------------------------------------------
[[ "$CLUSTER_NAME" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] \
    || fail "$EXIT_USAGE" "--cluster-name must be a DNS label, got: $CLUSTER_NAME"
[[ "$REG_NAME" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] \
    || fail "$EXIT_USAGE" "--registry-name must be a DNS label, got: $REG_NAME"
[[ "$REG_PORT" =~ ^[1-9][0-9]{2,4}$ ]] \
    || fail "$EXIT_USAGE" "--registry-port must be a port number, got: $REG_PORT"
[[ "$MIN_FREE_GIB" =~ ^[0-9]+$ ]] \
    || fail "$EXIT_USAGE" "--min-free-gib must be an integer, got: $MIN_FREE_GIB"

# --- Guard 2: reserved targets ----------------------------------------------
for reserved in "${RESERVED_CLUSTER_NAMES[@]}"; do
    [ "$CLUSTER_NAME" != "$reserved" ] || fail "$EXIT_REFUSED_TARGET" \
        "refusing to operate on kind cluster '$CLUSTER_NAME': it belongs to the developer's Tilt environment or to a sibling harness, and this script deletes what it is given"
done
for reserved in "${RESERVED_REGISTRY_NAMES[@]}"; do
    [ "$REG_NAME" != "$reserved" ] || fail "$EXIT_REFUSED_TARGET" \
        "refusing to operate on registry '$REG_NAME': it belongs to another harness"
done
for reserved in "${RESERVED_REG_PORTS[@]}"; do
    [ "$REG_PORT" != "$reserved" ] || fail "$EXIT_REFUSED_TARGET" \
        "refusing to publish on port $REG_PORT: another harness's registry already claims it"
done

# --- Guard 3: the version floor ---------------------------------------------
K8S_MAJOR=${KIND_IMAGE_VERSION%%.*}
K8S_REST=${KIND_IMAGE_VERSION#*.}
K8S_MINOR=${K8S_REST%%.*}
[[ "$K8S_MAJOR" =~ ^[0-9]+$ && "$K8S_MINOR" =~ ^[0-9]+$ ]] \
    || fail "$EXIT_USAGE" "--k8s-version must look like 1.35.0, got: $KIND_IMAGE_VERSION"
if [ "$K8S_MAJOR" -lt 1 ] || { [ "$K8S_MAJOR" -eq 1 ] && [ "$K8S_MINOR" -lt "$MIN_K8S_MINOR" ]; }; then
    fail "$EXIT_VERSION_FLOOR" \
        "Kubernetes $KIND_IMAGE_VERSION is below the 1.$MIN_K8S_MINOR floor this harness needs (pods/resize actuation and a containerd with cgroup_writable). The epic floor is 1.$EPIC_MIN_K8S_MINOR; the 1.29 in older docs was measured false and corrected in #2818."
fi

# --- Guard 4: the context is DERIVED, never discovered ----------------------
CONTEXT="kind-${CLUSTER_NAME}"
if [ -n "$REQUESTED_CONTEXT" ] && [ "$REQUESTED_CONTEXT" != "$CONTEXT" ]; then
    fail "$EXIT_REFUSED_TARGET" \
        "refusing --context '$REQUESTED_CONTEXT': this harness only ever targets '$CONTEXT', the context of the cluster it creates and deletes"
fi
KUBECTL=("$KUBECTL_BIN" --context "$CONTEXT")

# --- Guard 5: the resolved API server must be a local kind server -----------
#
# Guard 4 checks the NAME. This checks where that name points, which is what
# catches a kubeconfig entry called `kind-djinn-resize-omp4` aimed at EKS. Host
# anchored: `https://127.0.0.1.evil.example` starts with the loopback address
# and is a remote host. Skipped when the context does not exist yet, which is
# the normal state before `up`.
refuse_non_kind_server() {
    local server
    server=$("$KUBECTL_BIN" --context "$CONTEXT" config view --minify \
        -o 'jsonpath={.clusters[0].cluster.server}' 2>/dev/null || true)
    [ -n "$server" ] || return 0
    local host=${server#https://}
    host=${host%%/*}
    host=${host%:*}
    case "$host" in
        127.0.0.1|localhost|'[::1]') return 0 ;;
        *)
            fail "$EXIT_REFUSED_TARGET" \
                "refusing to operate on context '$CONTEXT': its API server is $server, not a local kind server. Every context in a Djinn developer's kubeconfig is a live EKS cluster."
            ;;
    esac
}
refuse_non_kind_server

if [ "$ACTION" = check ]; then
    printf 'PASS: guards accept cluster=%s context=%s registry=%s:%s k8s=%s namespace=%s sentinels=%s\n' \
        "$CLUSTER_NAME" "$CONTEXT" "$REG_NAME" "$REG_PORT" "$KIND_IMAGE_VERSION" \
        "$NAMESPACE" "$SENTINEL_DIR"
    exit 0
fi

# --- Teardown ---------------------------------------------------------------
teardown() {
    info "deleting kind cluster $CLUSTER_NAME"
    "$KIND" delete cluster --name "$CLUSTER_NAME" || true
    if "$DOCKER" inspect "$REG_NAME" >/dev/null 2>&1; then
        info "removing registry container $REG_NAME"
        "$DOCKER" rm -f "$REG_NAME" >/dev/null || true
    fi
    # Prove the deletion rather than asserting it. A teardown that reported
    # success while the cluster survived is how a "disposable" harness becomes a
    # permanent one on a host that is already at 85% disk.
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
    require_tool "$KIND"
    require_tool "$DOCKER"
    teardown
    exit 0
fi

if [ "$ACTION" = selfcheck ]; then
    # The harness self-test AC10 names: after a teardown, neither the cluster nor
    # the registry may exist. Run it after `down`, or after a failed `up` — it is
    # the check that catches a REMOVED teardown trap, which otherwise leaves a
    # cluster behind and is invisible until the host fills up.
    require_tool "$KIND"
    require_tool "$DOCKER"
    surviving=$("$KIND" get clusters 2>/dev/null | grep -Fx "$CLUSTER_NAME" || true)
    [ -z "$surviving" ] || fail 1 \
        "selfcheck: kind cluster $CLUSTER_NAME still exists; the teardown path did not run"
    ! "$DOCKER" inspect "$REG_NAME" >/dev/null 2>&1 || fail 1 \
        "selfcheck: registry container $REG_NAME still exists; the teardown path did not run"
    printf 'PASS: no %s cluster and no %s registry survive\n' "$CLUSTER_NAME" "$REG_NAME"
    exit 0
fi

# --- up ---------------------------------------------------------------------
require_tool "$KIND"
require_tool "$KUBECTL_BIN"
require_tool "$DOCKER"
require_tool helm
[ -x "$UPSTREAM_SETUP" ] || fail "$EXIT_MISSING_TOOL" \
    "$UPSTREAM_SETUP is missing or not executable; this harness delegates cluster creation to it rather than growing a second cgroup-writable installer"

# Guard 6: disk, before anything is pulled. A full disk here evicts the control
# plane and looks exactly like a flaky test.
FREE_GIB=$(df -Pk / | awk 'NR == 2 { printf "%d", $4 / 1048576 }')
[ -n "$FREE_GIB" ] || fail 1 "could not read free space on /"
[ "$FREE_GIB" -ge "$MIN_FREE_GIB" ] || fail "$EXIT_LOW_DISK" \
    "only ${FREE_GIB}GiB free on /; this harness needs at least ${MIN_FREE_GIB}GiB for three image classes plus a node image. Prune cargo-target-runs and docker images before retrying."

# Guard 7: never adopt. A pre-existing cluster under this name was created by
# something else, and this script's failure path deletes what it created.
if "$KIND" get clusters 2>/dev/null | grep -Fxq "$CLUSTER_NAME"; then
    fail "$EXIT_CLUSTER_EXISTS" \
        "kind cluster '$CLUSTER_NAME' already exists. This harness refuses to adopt a cluster it did not create — run 'scripts/kind/setup-resize-matrix-cluster.sh down' first."
fi

cleanup_on_failure() {
    local status=$?
    if [ "$status" -ne 0 ] && [ "$KEEP_ON_FAILURE" = false ]; then
        info "run failed (exit $status); tearing the disposable cluster down"
        teardown || true
    fi
    exit "$status"
}
trap cleanup_on_failure EXIT

info "delegating cluster creation to $(basename "$UPSTREAM_SETUP") --cgroup-writable"
"$UPSTREAM_SETUP" up \
    --cluster-name "$CLUSTER_NAME" \
    --registry-name "$REG_NAME" \
    --registry-port "$REG_PORT" \
    --k8s-version "$KIND_IMAGE_VERSION" \
    --min-free-gib "$MIN_FREE_GIB" \
    --context "$CONTEXT" \
    --cgroup-writable

# Guard 8: re-check the floor against the LIVE API server. A tag can lie; the
# server cannot.
LIVE_MINOR=$("${KUBECTL[@]}" version -o json | awk -F'"' '/"minor"/ { gsub(/[^0-9]/, "", $4); print $4; exit }')
[ -n "$LIVE_MINOR" ] || fail 1 "could not read the live API server minor version"
[ "$LIVE_MINOR" -ge "$MIN_K8S_MINOR" ] || fail "$EXIT_VERSION_FLOOR" \
    "the created cluster reports Kubernetes 1.${LIVE_MINOR}, below the 1.${MIN_K8S_MINOR} floor"

# The two capabilities the matrix cannot fake. Checked here so a missing one
# fails during setup with a name, rather than during a live cell as "the leaf
# quota never appeared".
info "verifying the cgroup-writable RuntimeClass is installed"
"${KUBECTL[@]}" get runtimeclass "$CGROUP_WRITABLE_RUNTIME_CLASS" >/dev/null 2>&1 \
    || fail 1 "RuntimeClass $CGROUP_WRITABLE_RUNTIME_CLASS is absent; the launcher cannot obtain a writable cgroup root and every leaf-authority cell would fail for the wrong reason"

info "verifying the pods/resize subresource is served"
"${KUBECTL[@]}" get --raw '/api/v1' \
    | grep -q '"name":"pods/resize"' \
    || fail "$EXIT_VERSION_FLOOR" "this API server does not serve pods/resize"

# The AC4 sentinel lands on the node, outside any Pod, so its absence survives
# the Pod that would have written it never existing.
info "preparing the sentinel directory $SENTINEL_DIR on every node"
for node in $("$KIND" get nodes --name "$CLUSTER_NAME"); do
    "$DOCKER" exec "$node" sh -c "rm -rf '$SENTINEL_DIR' && mkdir -p '$SENTINEL_DIR' && chmod 0777 '$SENTINEL_DIR'"
done

trap - EXIT

cat <<EOF

PASS: mixed-version matrix harness is up.
  context:    ${CONTEXT}
  registry:   localhost:${REG_PORT} (${REG_NAME})
  namespace:  ${NAMESPACE}
  k8s:        1.${LIVE_MINOR}
  sentinels:  ${SENTINEL_DIR}

Build the three image classes and load them:
  tests/fixtures/resize-matrix/build.sh ${CLUSTER_NAME}

Run the live matrix:
  DJINN_TEST_RESIZE_MATRIX=1 cargo test -p djinn-server --test task_run_resize_mixed_version -- --ignored --test-threads=1

Tear it down (do this whether the tests passed or failed):
  scripts/kind/setup-resize-matrix-cluster.sh down
  scripts/kind/setup-resize-matrix-cluster.sh selfcheck
EOF
