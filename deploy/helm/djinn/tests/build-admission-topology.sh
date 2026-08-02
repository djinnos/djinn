#!/usr/bin/env bash
# Helm render contract for the REMOVAL of build admission.
#
# The pods-quota reservation authority (the pre-create build-admission ledger)
# was deleted from the server, and with it the two env vars that configured it:
# DJINN_BUILD_ADMISSION_MODE and DJINN_MAX_BUILD_TASKRUNS. The single-active
# writer constraint that authority required is gone too, so the chart must no
# longer refuse to render a multi-replica, surge-capable server.
#
# This script is the inverse of the one it replaces. It asserts:
#   1. `--set buildAdmission.mode=enforce --set server.replicas=3` RENDERS.
#      A surviving `fail()` in templates/deployment-server.yaml fails here.
#   2. Neither removed env var appears anywhere in the render, under any value
#      combination — including the exact ones that used to emit them.
#   3. The stale `buildAdmission.*` flags are inert, not merely tolerated: the
#      render is byte-identical with and without them.
#
# Usage: bash deploy/helm/djinn/tests/build-admission-topology.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHART_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

require_tool() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "FAIL: required test tool '$1' is not installed" >&2
        exit 1
    }
}

require_tool helm
require_tool python3
TMPDIR_RENDER=$(mktemp -d)
trap 'rm -rf "$TMPDIR_RENDER"' EXIT

# The removed env vars. Named once so the greps below cannot drift apart.
REMOVED_ENV=(DJINN_BUILD_ADMISSION_MODE DJINN_MAX_BUILD_TASKRUNS DJINN_MAX_BUILD_PODS)

render() {
    local output=$1 name=$2
    shift 2
    if ! helm template build-admission-test "$CHART_DIR" \
        --is-upgrade \
        --show-only templates/deployment-server.yaml "$@" > "$output" 2> "$output.err"; then
        echo "FAIL: '$name' must render, but helm template refused:" >&2
        cat "$output.err" >&2
        exit 1
    fi
}

assert_no_removed_env() {
    local manifest=$1 name=$2
    local var
    for var in "${REMOVED_ENV[@]}"; do
        if grep -q -- "$var" "$manifest"; then
            echo "FAIL: '$name' still renders the removed env var $var:" >&2
            grep -n -- "$var" "$manifest" >&2
            exit 1
        fi
    done
}

# The Deployment must still be a well-formed Deployment with the server
# container's env list intact — a render that emitted nothing at all would
# satisfy the greps above vacuously.
assert_server_shape() {
    local manifest=$1 replicas=$2
    python3 - "$manifest" "$replicas" <<'PY'
import sys
import yaml

path, expected_replicas = sys.argv[1:]
docs = [doc for doc in yaml.safe_load_all(open(path, encoding="utf-8")) if doc]
deployments = [doc for doc in docs if doc.get("kind") == "Deployment"]
assert len(deployments) == 1, f"expected exactly one Deployment, got {len(deployments)}"
spec = deployments[0]["spec"]
assert str(spec["replicas"]) == expected_replicas, (
    f"expected replicas {expected_replicas}, got {spec['replicas']!r}"
)

containers = spec["template"]["spec"]["containers"]
server = next(container for container in containers if container["name"] == "djinn-server")
names = [entry["name"] for entry in server["env"]]
assert len(names) > 10, f"server env list looks truncated: {names}"
# Anchor on a neighbour of the deleted entries so an env block that silently
# stopped rendering cannot pass as "the vars are absent".
assert "DJINN_CACHE_CLEANUP_MODE" in names, "server env lost DJINN_CACHE_CLEANUP_MODE"
assert "DJINN_IMAGE_BUILDER_IMAGE" in names, "server env lost DJINN_IMAGE_BUILDER_IMAGE"
for removed in ("DJINN_BUILD_ADMISSION_MODE", "DJINN_MAX_BUILD_TASKRUNS", "DJINN_MAX_BUILD_PODS"):
    assert removed not in names, f"server env still carries removed entry {removed}"
PY
}

# --- The acceptance criterion, verbatim -------------------------------------
echo "=== enforce + replicas=3 renders (the topology the deleted ledger forbade) ==="
render "$TMPDIR_RENDER/enforce-replicas-3.yaml" "enforce+replicas=3" \
    --set buildAdmission.mode=enforce \
    --set server.replicas=3
assert_no_removed_env "$TMPDIR_RENDER/enforce-replicas-3.yaml" "enforce+replicas=3"
assert_server_shape "$TMPDIR_RENDER/enforce-replicas-3.yaml" 3

# --- Every topology the old fail() rejected ---------------------------------
echo "=== enforce + surge-capable RollingUpdate renders ==="
render "$TMPDIR_RENDER/enforce-surge.yaml" "enforce+surge" \
    --set-string buildAdmission.mode=enforce \
    --set server.replicas=2 \
    --set-string server.strategy.type=RollingUpdate \
    --set server.strategy.rollingUpdate.maxSurge=1 \
    --set server.strategy.rollingUpdate.maxUnavailable=0
assert_no_removed_env "$TMPDIR_RENDER/enforce-surge.yaml" "enforce+surge"
assert_server_shape "$TMPDIR_RENDER/enforce-surge.yaml" 2

echo "=== enforce + replicas=0 renders ==="
render "$TMPDIR_RENDER/enforce-zero.yaml" "enforce+replicas=0" \
    --set-string buildAdmission.mode=enforce \
    --set server.replicas=0
assert_no_removed_env "$TMPDIR_RENDER/enforce-zero.yaml" "enforce+replicas=0"
assert_server_shape "$TMPDIR_RENDER/enforce-zero.yaml" 0

# --- Defaults, with no buildAdmission flag in sight -------------------------
echo "=== chart defaults render without either removed env var ==="
render "$TMPDIR_RENDER/defaults.yaml" "defaults"
assert_no_removed_env "$TMPDIR_RENDER/defaults.yaml" "defaults"
assert_server_shape "$TMPDIR_RENDER/defaults.yaml" 1

# --- The stale flags are inert, not just tolerated --------------------------
# A schema that still validated buildAdmission, or a template that still read
# it, would show up as a diff here.
echo "=== stale buildAdmission.* flags do not change the render ==="
render "$TMPDIR_RENDER/stale-flags.yaml" "stale flags" \
    --set-string buildAdmission.mode=enforce \
    --set buildAdmission.maxBuildTaskRuns=64
# The vault-key Secret is generated with randAlphaNum, so its checksum
# annotation differs between two renders of the SAME values. Drop that one line
# before comparing; everything else is deterministic.
sanitize() { grep -v 'checksum/secret-vault-key:' "$1"; }
sanitize "$TMPDIR_RENDER/defaults.yaml" > "$TMPDIR_RENDER/defaults.stable"
sanitize "$TMPDIR_RENDER/stale-flags.yaml" > "$TMPDIR_RENDER/stale-flags.stable"
if ! diff -u "$TMPDIR_RENDER/defaults.stable" "$TMPDIR_RENDER/stale-flags.stable" > "$TMPDIR_RENDER/stale.diff"; then
    echo "FAIL: buildAdmission.* still influences the render:" >&2
    cat "$TMPDIR_RENDER/stale.diff" >&2
    exit 1
fi

# The old chart rejected these outright (empty mode, zero cap, cap>64). They
# must now be accepted and ignored, which is what proves the values.schema.json
# constraints were retired along with the template guard.
echo "=== formerly-rejected buildAdmission values are now inert ==="
render "$TMPDIR_RENDER/inert-empty-mode.yaml" "empty mode" --set-string buildAdmission.mode=
assert_no_removed_env "$TMPDIR_RENDER/inert-empty-mode.yaml" "empty mode"
render "$TMPDIR_RENDER/inert-zero-cap.yaml" "zero cap" --set buildAdmission.maxBuildTaskRuns=0
assert_no_removed_env "$TMPDIR_RENDER/inert-zero-cap.yaml" "zero cap"
render "$TMPDIR_RENDER/inert-oversized-cap.yaml" "oversized cap" --set buildAdmission.maxBuildTaskRuns=65
assert_no_removed_env "$TMPDIR_RENDER/inert-oversized-cap.yaml" "oversized cap"

# --- Chart-wide sweep: no template anywhere reintroduces the vars -----------
echo "=== full-chart render carries neither removed env var ==="
helm template build-admission-test "$CHART_DIR" --is-upgrade \
    --set buildAdmission.mode=enforce \
    --set server.replicas=3 \
    --set kueue.enabled=false --set kueue.armed=false \
    > "$TMPDIR_RENDER/full.yaml"
assert_no_removed_env "$TMPDIR_RENDER/full.yaml" "full chart"

echo "=== All build-admission removal Helm render tests passed ==="
