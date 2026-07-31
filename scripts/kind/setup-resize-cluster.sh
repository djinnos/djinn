#!/usr/bin/env bash
# Disposable kind cluster for the in-place Pod resize harness (0ppk-1b, AC8).
#
# This creates and DELETES its target. Everything about it is therefore
# harness-specific and nothing about it is discovered: the cluster name, the
# registry name, the registry port and the context are all derived from
# constants below, never read out of the ambient kubeconfig. On a Djinn
# developer's machine every context in the kubeconfig is a live EKS cluster, so
# "use the current context" is not a convenience here, it is a production
# outage.
#
# Do NOT model changes on scripts/kind/setup-kind.sh. That script manages the
# developer's PERSISTENT Tilt cluster and never deletes anything. This one is
# modelled on scripts/kind/setup-kueue-cluster.sh, which is the disposable
# harness, and it deliberately reserves that harness's names too so the two can
# never fight over a cluster.
#
# WHY A SEPARATE CLUSTER FROM THE KUEUE HARNESS
#
# In-place Pod vertical scaling needs a newer Kubernetes than the Kueue harness
# pins, and the resize tests mutate live Pod resources. Sharing a cluster would
# mean one harness's feature-gate requirement silently governing the other's,
# and a resize test's Pod mutations landing inside the other's Workload
# accounting.
#
# Usage:
#   scripts/kind/setup-resize-cluster.sh up      # create (default)
#   scripts/kind/setup-resize-cluster.sh down    # delete cluster AND registry
#   scripts/kind/setup-resize-cluster.sh check   # run every guard, create nothing
#
# Options:
#   --context NAME         must equal the derived context; any other value is refused
#   --cluster-name NAME    default djinn-resize-harness
#   --registry-name NAME   default djinn-resize-harness-registry
#   --registry-port PORT   default 5052
#   --k8s-version VER      default 1.33.1
#   --min-free-gib N       default 20
#   --keep-on-failure      skip the failure-path teardown (debugging only)
#
# Exit codes:
#   2   usage error
#   3   refused target (reserved name, or a context this harness does not own)
#   4   not enough free disk
#   5   a required tool is missing
#   6   refused to adopt a pre-existing cluster
#   7   Kubernetes version below the in-place-resize floor
#   1   anything else (create failure, API error, ...)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

KIND="${KIND:-kind}"
KUBECTL_BIN="${KUBECTL:-kubectl}"
DOCKER="${DOCKER:-docker}"

# Harness-specific everything. None of these may collide with the Tilt cluster
# or with the Kueue harness.
CLUSTER_NAME="${DJINN_RESIZE_HARNESS_CLUSTER:-djinn-resize-harness}"
REG_NAME="${DJINN_RESIZE_HARNESS_REGISTRY:-djinn-resize-harness-registry}"
REG_PORT="${DJINN_RESIZE_HARNESS_REG_PORT:-5052}"
KIND_IMAGE_VERSION="${DJINN_RESIZE_HARNESS_K8S_VERSION:-1.33.1}"
MIN_FREE_GIB="${DJINN_RESIZE_HARNESS_MIN_FREE_GIB:-20}"
READY_TIMEOUT_SECONDS="${DJINN_RESIZE_HARNESS_READY_TIMEOUT_SECONDS:-300}"
REQUESTED_CONTEXT=""
KEEP_ON_FAILURE=false
ACTION=""

# The names this script must never touch, whatever it is asked to do.
#
# `djinn` / `kind-registry` / 5001 are scripts/kind/setup-kind.sh's defaults,
# i.e. the developer's live Tilt environment. The `djinn-kueue-harness` entries
# are the OTHER disposable harness: this script deletes what it is given, and a
# `down` aimed at the Kueue harness would destroy a run in progress.
RESERVED_CLUSTER_NAMES=(djinn kind djinn-kueue-harness)
RESERVED_REGISTRY_NAMES=(kind-registry djinn-kueue-harness-registry)
RESERVED_REG_PORTS=(5000 5001 5051)

EXIT_USAGE=2
EXIT_REFUSED_TARGET=3
EXIT_LOW_DISK=4
EXIT_MISSING_TOOL=5
EXIT_CLUSTER_EXISTS=6
EXIT_VERSION_FLOOR=7

# `pods/resize` is the subresource this harness exists to exercise, and
# `InPlacePodVerticalScaling` is beta and enabled by default from 1.33. Below
# that the PATCH either 404s (no subresource) or needs an alpha gate the node
# image may not carry — and a harness that silently ran without the feature
# would report "not confirmed" for a reason that has nothing to do with the code
# under test.
MIN_K8S_MINOR=33

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

# Guard 1: reserved names. The Tilt cluster and the Kueue harness are not
# "unlikely to be selected"; they are unselectable.
for reserved in "${RESERVED_CLUSTER_NAMES[@]}"; do
    [ "$CLUSTER_NAME" != "$reserved" ] || fail "$EXIT_REFUSED_TARGET" \
        "refusing to operate on kind cluster '$CLUSTER_NAME': that name belongs to the developer's Tilt cluster or to the Kueue harness. This harness creates and DELETES its target, so it must own the name outright."
done
for reserved in "${RESERVED_REGISTRY_NAMES[@]}"; do
    [ "$REG_NAME" != "$reserved" ] || fail "$EXIT_REFUSED_TARGET" \
        "refusing to operate on registry '$REG_NAME': that registry belongs to the Tilt environment or to the Kueue harness, and 'down' deletes the registry it is given."
done
for reserved in "${RESERVED_REG_PORTS[@]}"; do
    [ "$REG_PORT" != "$reserved" ] || fail "$EXIT_REFUSED_TARGET" \
        "refusing registry port $REG_PORT: it is already published by the Tilt registry or by the Kueue harness, and binding it would either fail or shadow it."
done

# Guard 2: the Kubernetes floor for in-place resize.
K8S_MAJOR=${KIND_IMAGE_VERSION%%.*}
K8S_REST=${KIND_IMAGE_VERSION#*.}
K8S_MINOR=${K8S_REST%%.*}
[[ "$K8S_MAJOR" =~ ^[0-9]+$ && "$K8S_MINOR" =~ ^[0-9]+$ ]] \
    || fail "$EXIT_USAGE" "--k8s-version must look like 1.33.1, got: $KIND_IMAGE_VERSION"
if [ "$K8S_MAJOR" -lt 1 ] || { [ "$K8S_MAJOR" -eq 1 ] && [ "$K8S_MINOR" -lt "$MIN_K8S_MINOR" ]; }; then
    fail "$EXIT_VERSION_FLOOR" \
        "Kubernetes $KIND_IMAGE_VERSION is below the 1.$MIN_K8S_MINOR floor the pods/resize subresource needs. A lower version does not fail loudly; it 404s the subresource or leaves the resize unactuated, which reads exactly like the bug this harness exists to catch."
fi

# Guard 3: the context. Derived, never discovered.
CONTEXT="kind-${CLUSTER_NAME}"
if [ -n "$REQUESTED_CONTEXT" ] && [ "$REQUESTED_CONTEXT" != "$CONTEXT" ]; then
    fail "$EXIT_REFUSED_TARGET" \
        "refusing --context '$REQUESTED_CONTEXT': this harness only ever targets '$CONTEXT', the context of the kind cluster it creates and deletes itself. Every context in a Djinn developer's kubeconfig today is a live EKS cluster (demo/staging/prod)."
fi

KUBECTL=("$KUBECTL_BIN" --context "$CONTEXT")

if [ "$ACTION" = check ]; then
    printf 'PASS: guards accept cluster=%s context=%s registry=%s:%s k8s=%s\n' \
        "$CLUSTER_NAME" "$CONTEXT" "$REG_NAME" "$REG_PORT" "$KIND_IMAGE_VERSION"
    exit 0
fi

require_tool() {
    command -v "$1" >/dev/null 2>&1 || fail "$EXIT_MISSING_TOOL" "$1 is not on PATH; this harness needs docker, kind and kubectl"
}
require_tool "$DOCKER"
require_tool "$KIND"
require_tool "$KUBECTL_BIN"

# --- Teardown ---------------------------------------------------------------
# Deletes only the cluster and registry NAMES validated above. Called by `down`
# and by `up`'s failure trap, so the cluster and the registry are gone whether
# the run passed or failed.
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

# Guard 4: disk. Checked before anything is pulled, because the failure mode of
# a full disk here is an evicted control plane that looks like a flaky test.
FREE_GIB=$(df -Pk / | awk 'NR == 2 { printf "%d", $4 / 1048576 }')
[ "$FREE_GIB" -ge "$MIN_FREE_GIB" ] || fail "$EXIT_LOW_DISK" \
    "only ${FREE_GIB}GiB free on /; this harness needs at least ${MIN_FREE_GIB}GiB. Prune cargo-target-runs and docker images before retrying."

# Guard 5: never adopt. A pre-existing cluster under this name was created by
# something else, and `up` ends by deleting what it created.
if "$KIND" get clusters 2>/dev/null | grep -Fxq "$CLUSTER_NAME"; then
    fail "$EXIT_CLUSTER_EXISTS" \
        "kind cluster '$CLUSTER_NAME' already exists. This harness refuses to adopt a cluster it did not create — run 'scripts/kind/setup-resize-cluster.sh down' first."
fi

cleanup_on_failure() {
    local status=$?
    if [ "$status" -ne 0 ] && [ "$KEEP_ON_FAILURE" = false ]; then
        info "run failed (exit $status); tearing the harness down"
        teardown || true
    fi
    exit "$status"
}
trap cleanup_on_failure EXIT

info "starting local registry ${REG_NAME} on :${REG_PORT}"
if ! "$DOCKER" inspect "$REG_NAME" >/dev/null 2>&1; then
    "$DOCKER" run -d --restart=always -p "127.0.0.1:${REG_PORT}:5000" \
        --name "$REG_NAME" registry:2 >/dev/null
fi

# Guard 5b: state the feature gate only where stating it is legal.
#
# `InPlacePodVerticalScaling` is beta-on-by-default at 1.33 and GA from 1.34
# (measured 2026-07-31: a 1.35.0 kubelet logs "Setting GA feature gate
# InPlacePodVerticalScaling=true. It will be removed in a future release").
# Naming a GA gate is tolerated with a deprecation warning until it is removed,
# and naming a REMOVED gate is a hard kubelet start failure — so an
# unconditional `featureGates:` block silently pins this harness to a closing
# window of node images. Below the GA minor the explicit statement still earns
# its place: it is the only thing standing between a node image whose beta
# default flipped and a run that reports "not confirmed" for a reason that has
# nothing to do with the code under test.
GA_K8S_MINOR=34
FEATURE_GATES_BLOCK=""
if [ "$K8S_MINOR" -lt "$GA_K8S_MINOR" ]; then
    FEATURE_GATES_BLOCK=$'featureGates:\n  InPlacePodVerticalScaling: true'
    info "k8s 1.${K8S_MINOR}: stating InPlacePodVerticalScaling=true explicitly (beta)"
else
    info "k8s 1.${K8S_MINOR}: InPlacePodVerticalScaling is GA; naming it would only earn a removal warning"
fi

info "creating kind cluster ${CLUSTER_NAME} (k8s ${KIND_IMAGE_VERSION})"
"$KIND" create cluster --name "$CLUSTER_NAME" --image "kindest/node:v${KIND_IMAGE_VERSION}" \
    --config /dev/stdin <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
${FEATURE_GATES_BLOCK}
# The registry is wired through containerd's per-host \`certs.d\` directory, NOT
# through a \`registry.mirrors\` table. kind's node image already sets
# \`config_path\`, and containerd v2 refuses to load its CRI image plugin at all
# when both are present: "\`mirrors\` cannot be set when \`config_path\` is
# provided". That refusal is total and silent from the outside — the CRI service
# never registers, the kubelet dies on "unknown service runtime.v1.RuntimeService",
# and \`kind create\` fails four minutes later at "waiting for a healthy kubelet"
# with nothing in its output naming containerd. Restating \`config_path\` here is
# what \`scripts/kind/setup-kueue-cluster.sh\` does and is the only shape that
# works on a containerd v2 node.
containerdConfigPatches:
  - |-
    [plugins."io.containerd.grpc.v1.cri".registry]
      config_path = "/etc/containerd/certs.d"
nodes:
  - role: control-plane
EOF

info "connecting the registry to the kind network"
if [ "$("$DOCKER" inspect -f '{{json .NetworkSettings.Networks.kind}}' "$REG_NAME")" = 'null' ]; then
    "$DOCKER" network connect kind "$REG_NAME" >/dev/null
fi

# Both spellings, because a manifest may name the registry either from the host
# (`localhost:$REG_PORT`) or from inside the cluster (`$REG_NAME:5000`).
info "pointing the node's certs.d at the registry"
REGISTRY_HOST_DIR="/etc/containerd/certs.d/localhost:${REG_PORT}"
REGISTRY_INTERNAL_DIR="/etc/containerd/certs.d/${REG_NAME}:5000"
for node in $("$KIND" get nodes --name "$CLUSTER_NAME"); do
    "$DOCKER" exec "$node" mkdir -p "$REGISTRY_HOST_DIR" "$REGISTRY_INTERNAL_DIR"
    "$DOCKER" exec -i "$node" cp /dev/stdin "$REGISTRY_HOST_DIR/hosts.toml" <<EOF
[host."http://${REG_NAME}:5000"]
  capabilities = ["pull", "resolve"]
EOF
    "$DOCKER" exec -i "$node" cp /dev/stdin "$REGISTRY_INTERNAL_DIR/hosts.toml" <<EOF
[host."http://${REG_NAME}:5000"]
  capabilities = ["pull", "resolve"]
EOF
done

info "waiting for the node to become Ready"
"${KUBECTL[@]}" wait --for=condition=Ready node --all \
    --timeout="${READY_TIMEOUT_SECONDS}s" >/dev/null

# Guard 6: re-check the floor against the LIVE API server, not the requested
# image tag. A tag can lie; the server cannot.
#
# Read through `get --raw /version`, which returns the SERVER's version object and
# nothing else. `kubectl version -o json` emits `clientVersion` FIRST and
# `serverVersion` second, so the obvious `awk '/"minor"/ { ...; exit }'` over it
# reads the local kubectl's minor and calls it the cluster's. That is not a
# cosmetic slip: it is a floor guard measuring the wrong ledger, and it would
# have waved through a 1.20 API server on any developer machine with a current
# kubectl — exactly the "the tag can lie" case this guard exists for. Observed
# live: a 1.33.1 cluster reported as "k8s: 1.36".
LIVE_MINOR=$("${KUBECTL[@]}" get --raw /version | awk -F'"' '/"minor"/ { gsub(/[^0-9]/, "", $4); print $4; exit }')
[ -n "$LIVE_MINOR" ] || fail 1 "could not read the live API server minor version"
[ "$LIVE_MINOR" -ge "$MIN_K8S_MINOR" ] || fail "$EXIT_VERSION_FLOOR" \
    "the created cluster reports Kubernetes 1.${LIVE_MINOR}, below the 1.${MIN_K8S_MINOR} in-place-resize floor"

trap - EXIT

cat <<EOF

PASS: resize harness is up.
  context:   ${CONTEXT}
  registry:  localhost:${REG_PORT} (${REG_NAME})
  k8s:       1.${LIVE_MINOR}

Run the live tests:
  DJINN_TEST_RESIZE_CLUSTER=1 cargo test -p djinn-k8s --test pod_resize_kind -- --ignored

Tear it down (do this whether the tests passed or failed):
  scripts/kind/setup-resize-cluster.sh down
EOF
