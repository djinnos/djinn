#!/bin/sh
# Render the Tiltfile with isolated overrides without touching Docker or kind.
#
#   sh scripts/test-tiltfile-config.sh

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

command -v tilt >/dev/null 2>&1 || {
    printf 'FATAL: tilt is required\n' >&2
    exit 2
}
command -v helm >/dev/null 2>&1 || {
    printf 'FATAL: helm is required\n' >&2
    exit 2
}

umask 077
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/djinn-tiltfile-config.XXXXXX")
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM
RESULT="$TMP_DIR/result.json"

cd "$REPO_ROOT"
tilt alpha tiltfile-result \
    --context kind-djinn-validation \
    -- \
    --bootstrap-cluster=false \
    --cluster-name djinn-validation \
    --registry-name kind-registry-validation \
    --registry-port 15001 \
    --state-dir "$TMP_DIR/state" \
    --api-port 13000 \
    --rpc-port 18443 \
    --postgres-port 15432 \
    --qdrant-http-port 16333 \
    --qdrant-grpc-port 16334 \
    --langfuse-port 15000 \
    --minio-port 19091 > "$RESULT"

assert_contains() {
    label=$1
    needle=$2
    file=$3
    if ! grep -Fq -- "$needle" "$file"; then
        printf 'FAIL: %s (missing %s)\n' "$label" "$needle" >&2
        exit 1
    fi
    printf 'ok: %s\n' "$label"
}

assert_lacks() {
    label=$1
    needle=$2
    file=$3
    if grep -Fq -- "$needle" "$file"; then
        printf 'FAIL: %s (unexpected %s)\n' "$label" "$needle" >&2
        exit 1
    fi
    printf 'ok: %s\n' "$label"
}

assert_contains 'host registry override reaches the server image' \
    'localhost:15001/djinn-server' "$RESULT"
assert_contains 'in-cluster registry override reaches the runtime image' \
    'kind-registry-validation:5000/djinn-agent-runtime' "$RESULT"
assert_contains 'in-cluster registry override reaches the builder image' \
    'kind-registry-validation:5000/djinn-image-builder' "$RESULT"
assert_contains 'insecure BuildKit registry override is rendered' \
    '[registry.\"kind-registry-validation:5000\"]' "$RESULT"
assert_contains 'public URL follows the API host port' \
    'DJINN_PUBLIC_URL: http://localhost:13000' "$RESULT"
assert_contains 'web URL follows the API host port' \
    'DJINN_WEB_URL: http://localhost:13000' "$RESULT"
assert_contains 'Langfuse callback URL follows its host port' \
    'http://localhost:15000' "$RESULT"
assert_contains 'API port-forward override is registered' \
    '"localPort": 13000' "$RESULT"
assert_contains 'MinIO port-forward override is registered' \
    '"localPort": 19091' "$RESULT"
assert_contains 'isolated Cargo registry cache is scoped by cluster' \
    'djinn-cargo-registry-djinn-validation' "$RESULT"
assert_contains 'isolated Cargo target cache is scoped by cluster' \
    'djinn-cargo-target-djinn-validation' "$RESULT"
assert_contains 'isolated sccache is scoped by cluster' \
    'djinn-sccache-djinn-validation' "$RESULT"
assert_contains 'isolated runtime base image is scoped by cluster' \
    'djinn-agent-runtime-base:djinn-validation' "$RESULT"
assert_lacks 'fresh isolated render omits GitHub App env surface' \
    'GITHUB_APP_' "$RESULT"

collision_log="$TMP_DIR/collision.log"
if tilt alpha tiltfile-result -- \
    --api-port 3000 --rpc-port 3000 > "$collision_log" 2>&1; then
    printf 'FAIL: duplicate host ports were accepted\n' >&2
    exit 1
fi
assert_contains 'duplicate host ports fail before bootstrap' \
    '--api-port and --rpc-port both use host port 3000' "$collision_log"

printf 'all Tiltfile isolation assertions passed\n'
